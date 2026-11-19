//! Port of `src/commands/admin/automod.js`: `/automod` (MEE6 Moderator › Auto-moderation). An admin config command
//! (`gated = false`): rules, actions and exemptions, the word, pattern and domain lists, and the honeypot channels.
//! Replies for `words` and `patterns` echo slurs and filter-evasion patterns, so they are ephemeral.

use std::sync::{Arc, Mutex};

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, Payload, PermTarget, check_channel_usable, missing_bot_permissions, respond_autocomplete, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::safe_regex::{compile_safe_regex, safe_regex_test, validate_regex};
use bot_util::text::{chunk_lines, clamp_text, default_more, join_with_limit};
use serenity::all::{ChannelId, ChannelType, GuildId, Permissions};
use serenity::builder::{CreateAllowedMentions, CreateChannel, CreateEmbed, CreateMessage};

use crate::automod_service::{
    self as svc, AutomodConfig, EXEMPT_CAP, HONEYPOT_CAP, HONEYPOT_EMOJI, HONEYPOT_LABEL, MAX_TIMEOUT_SECONDS, MIN_TIMEOUT_SECONDS, RULE_KEYS, action_label,
    describe_honeypot, describe_honeypot_value, describe_links_mode, describe_rule_line, get_automod_config, honeypot_action_label, is_deleting_action,
    is_legacy_config, normalize_config, normalize_domain, normalize_word, resolve_rule_key, rule_meta, strip_invisible,
};

const EXEMPT_CHANNEL_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_FORUM,
    channel_type::GUILD_MEDIA,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::GUILD_CATEGORY,
];

/// A honeypot is a channel members can type in; never a category, forum or voice channel.
const HONEYPOT_CHANNEL_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];
pub const HONEYPOT_CHANNEL_NAME: &str = "do-not-post";

fn action_choices(o: Opt) -> Opt {
    o.choice("Off", "off")
        .choice("Delete message", "delete")
        .choice("Warn member", "warn")
        .choice("Delete & warn", "delete_warn")
        .choice("Delete & timeout", "timeout")
        .choice("Delete & kick", "kick")
        .choice("Delete & ban", "ban")
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("automod", "Auto-moderation rules, actions and exemptions (MEE6 Moderator › Auto-moderation).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("enable", "Turn automod on. Your rules and lists stay as they are."))
        .option(Opt::sub_command("disable", "Turn automod off. Rules and lists are kept."))
        .option(Opt::sub_command("status", "Show every rule with its action and scope, the lists and the exemptions."))
        .option(
            Opt::sub_command("rule", "Set a rule's action, threshold and scope (Bad Words, Caps, Anti-Spam, …).")
                .option(Opt::string("rule", "The rule to configure.").required(true).autocomplete(true))
                .option(action_choices(Opt::string("action", "What happens when the rule matches.").required(true)))
                .option(Opt::integer("threshold", "Trigger level: caps %, emoji/spoiler/mention count, repeats, messages or marks.").min_value(1).max_value(100))
                .option(Opt::string("timeout", "Timeout length for the timeout action, e.g. 10m, 2h, 1d (default 10m, max 28d).").max_length(20))
                .option(
                    Opt::string("roles", "Which members the rule applies to.")
                        .choice("Everyone except the listed roles", "except")
                        .choice("Only the listed roles", "only"),
                )
                .option(
                    Opt::string("channels", "Which channels the rule applies in.")
                        .choice("Every channel except the listed ones", "except")
                        .choice("Only the listed channels", "only"),
                ),
        )
        .option(
            Opt::sub_command_group("words", "Bad Words rule: words and phrases matched as whole words.")
                .option(
                    Opt::sub_command("add", "Add words or phrases (comma-separated).")
                        .option(Opt::string("words", "Words or phrases, separated by commas.").required(true).max_length(1000)),
                )
                .option(
                    Opt::sub_command("remove", "Remove a word or phrase.")
                        .option(Opt::string("word", "The word or phrase to remove.").required(true).autocomplete(true).max_length(64)),
                )
                .option(Opt::sub_command("list", "List the filtered words (only you can see the list).")),
        )
        .option(
            Opt::sub_command_group("patterns", "Bad Words rule: regex patterns (checked for ReDoS before saving).")
                .option(
                    Opt::sub_command("add", "Add a case-insensitive regex pattern.")
                        .option(Opt::string("pattern", "Regex, e.g. fr[e3]{2}\\s*n[i1]tro").required(true).max_length(200)),
                )
                .option(
                    Opt::sub_command("remove", "Remove a regex pattern.")
                        .option(Opt::string("pattern", "The pattern to remove (pick from the list).").required(true).autocomplete(true).max_length(200)),
                )
                .option(Opt::sub_command("list", "List the regex patterns (only you can see the list).")),
        )
        .option(
            Opt::sub_command_group("domains", "External Links rule: blocked and allowed domains.")
                .option(
                    Opt::sub_command("add", "Add a domain to the blocked or allowed list.")
                        .option(Opt::string("domain", "Domain or URL, e.g. example.com (subdomains included).").required(true).max_length(100))
                        .option(Opt::string("list", "Which list (default: blocked).").choice("Blocked", "blocked").choice("Allowed", "allowed")),
                )
                .option(
                    Opt::sub_command("remove", "Remove a domain from the blocked and allowed lists.")
                        .option(Opt::string("domain", "The domain to remove.").required(true).autocomplete(true).max_length(100)),
                )
                .option(Opt::sub_command("list", "Show the blocked and allowed domains.")),
        )
        .option(
            Opt::sub_command_group("exempt", "Exempt roles or channels from every rule, or edit one rule's lists.")
                .option(
                    Opt::sub_command("add", "Add a role and/or channel to the exemptions or to a rule's list.")
                        .option(Opt::role("role", "Role to add."))
                        .option(Opt::channel("channel", "Channel or category to add (threads follow their parent).").channel_types(&EXEMPT_CHANNEL_TYPES))
                        .option(Opt::string("rule", "Only this rule's list (leave empty for all rules).").autocomplete(true)),
                )
                .option(
                    Opt::sub_command("remove", "Remove a role and/or channel from the exemptions or a rule's list.")
                        .option(Opt::role("role", "Role to remove."))
                        .option(Opt::channel("channel", "Channel or category to remove.").channel_types(&EXEMPT_CHANNEL_TYPES))
                        .option(Opt::string("rule", "Only this rule's list (leave empty for all rules).").autocomplete(true)),
                ),
        )
        .option(
            Opt::sub_command_group("honeypot", "Trap channels for spam bots: whoever posts in one is punished at once.")
                .option(
                    Opt::sub_command("add", "Make a channel a honeypot, or create one.")
                        .option(
                            Opt::channel("channel", format!("Leave empty to create #{HONEYPOT_CHANNEL_NAME} at the top.")).channel_types(&HONEYPOT_CHANNEL_TYPES),
                        )
                        .option(
                            Opt::string("action", "What happens to whoever posts there.")
                                .choice("Softban (default)", "softban")
                                .choice("Ban", "ban")
                                .choice("Kick", "kick")
                                .choice("Timeout", "timeout"),
                        )
                        .option(Opt::string("timeout", "Timeout length, e.g. 1h or 1d (default 1d).").max_length(20))
                        .option(Opt::boolean("warning", "Post a warning in the channel (default: yes).")),
                )
                .option(
                    Opt::sub_command("remove", "Stop a channel being a honeypot (the channel is kept).")
                        .option(Opt::channel("channel", "The honeypot.").required(true).channel_types(&HONEYPOT_CHANNEL_TYPES)),
                ),
        )
}

/// The command (`module 'automod'`, `gated = false`, ADMIN, with autocomplete).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Automod), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// "Bad Words, Repeated Text, … or Anti-Spam".
pub fn rule_names() -> String {
    let labels: Vec<&str> = svc::RULES.iter().map(|m| m.label).collect();
    let (last, rest) = labels.split_last().map(|(l, r)| (*l, r.to_vec())).unwrap_or(("", Vec::new()));
    format!("{} or {last}", rest.join(", "))
}

fn unknown_rule() -> CreateEmbed {
    error_embed("Unknown Rule", Some(&format!("Pick a rule from the list: {}.", rule_names())), None)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn save_failed() -> CreateEmbed {
    error_embed("Could Not Save", Some("The automod settings could not be saved right now. Please try again in a moment."), None)
}

/// Read-modify-write of `automodConfig` under the settings row lock. `mutate(cfg, report)` is pure: it edits the
/// normalised copy, fills `report`, and returns false when there is nothing to write.
pub async fn write_config<R, F>(app: &App, guild_id: &str, mutate: F) -> anyhow::Result<(Settings, R)>
where
    R: Default + Clone + Send + 'static,
    F: FnOnce(&mut AutomodConfig, &mut R) -> bool + Send + 'static,
{
    let report: Arc<Mutex<R>> = Arc::new(Mutex::new(R::default()));
    let rep = report.clone();
    let settings = app
        .settings()
        .update_settings_with(guild_id, move |current| {
            let mut r = R::default(); // a re-run must not see stale notes
            let mut cfg = normalize_config(current.automod_config.as_ref());
            let changed = mutate(&mut cfg, &mut r);
            *rep.lock().unwrap_or_else(|e| e.into_inner()) = r;
            Ok(if changed { SettingsPatch::new().set("automodConfig", cfg.to_json()) } else { SettingsPatch::new() })
        })
        .await?;
    let r = report.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok((settings, r))
}

fn off_hint(key: &str, config: &AutomodConfig) -> String {
    if config.rule(key).action == "off" {
        let label = rule_meta(key).map(|m| m.label).unwrap_or(key);
        format!("\nThe **{label}** rule is off. Turn it on with `/automod rule rule:{key} action:…`.")
    } else {
        String::new()
    }
}

fn role_mentions(ids: &[String], max: usize) -> String {
    let items: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
    join_with_limit(&items, ", ", max, default_more)
}

fn channel_mentions(ids: &[String], max: usize) -> String {
    let items: Vec<String> = ids.iter().map(|id| format!("<#{id}>")).collect();
    join_with_limit(&items, ", ", max, default_more)
}

fn len16(s: &str) -> usize {
    bot_util::js::utf16_len(s)
}

/// Packs whole items into pages of at most `max` characters, joined with `sep`.
pub fn pack_items(items: &[String], sep: &str, max: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut current = String::new();
    for item in items {
        let piece = clamp_text(item, max);
        if current.is_empty() {
            current = piece;
        } else if len16(&current) + len16(sep) + len16(&piece) <= max {
            current.push_str(sep);
            current.push_str(&piece);
        } else {
            pages.push(std::mem::replace(&mut current, piece));
        }
    }
    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

pub const NO_RULES_PROBLEM: &str = "No rules are active. Use `/automod rule` to turn one on.";

fn bot_missing(app: &App, guild_id: GuildId, perm: &str) -> bool {
    !missing_bot_permissions(app, PermTarget::Guild(guild_id), &[perm]).is_empty()
}

/// What would stop automod from working as configured, over the active rules (every rule, or just `only`).
pub fn collect_problems(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>, config: &AutomodConfig, only: Option<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    let keys: Vec<&str> = match only {
        Some(k) => vec![k],
        None => RULE_KEYS.to_vec(),
    }
    .into_iter()
    .filter(|k| config.rule(k).action != "off")
    .collect();
    let actions: Vec<String> = keys.iter().map(|k| config.rule(k).action).collect();
    let has = |a: &str| actions.iter().any(|x| x == a);

    if actions.iter().any(|a| is_deleting_action(a)) && bot_missing(app, guild_id, "ManageMessages") {
        problems.push("I need **Manage Messages** to delete messages.".to_string());
    }
    if has("timeout") && bot_missing(app, guild_id, "ModerateMembers") {
        problems.push("I need **Timeout Members** for the timeout action.".into());
    }
    if has("kick") && bot_missing(app, guild_id, "KickMembers") {
        problems.push("I need **Kick Members** for the kick action.".into());
    }
    if has("ban") && bot_missing(app, guild_id, "BanMembers") {
        problems.push("I need **Ban Members** for the ban action.".into());
    }
    if (keys.contains(&"spam") || keys.contains(&"repeatedText")) && !app.cache().available() {
        problems.push("Redis is unavailable right now, so Anti-Spam and Repeated Text are skipped until it is back.".into());
    }
    if keys.contains(&"badWords") && config.bad_words.is_empty() && config.bad_word_patterns.is_empty() {
        problems.push("**Bad Words** has no words or patterns yet — add some with `/automod words add`.".into());
    }
    for k in &keys {
        let r = config.rule(k);
        let label = rule_meta(k).map(|m| m.label).unwrap_or(k);
        if r.role_mode == "only" && r.role_ids.is_empty() {
            problems.push(format!("**{label}** applies to nobody: its roles mode is \"only listed\" but the list is empty."));
        }
        if r.channel_mode == "only" && r.channel_ids.is_empty() {
            problems.push(format!("**{label}** applies to nobody: its channels mode is \"only listed\" but the list is empty."));
        }
    }
    if only.is_none() && settings.is_some_and(|s| s.automod_enabled) && keys.is_empty() && config.honeypot.channel_ids.is_empty() {
        problems.push(NO_RULES_PROBLEM.into());
    }
    if only.is_none() {
        problems.extend(collect_honeypot_problems(app, guild_id, config));
    }
    if only.is_none() && settings.and_then(|s| s.mod_log_channel_id.as_deref()).is_none_or(str::is_empty) {
        problems.push("No mod-log channel — set one with `/modconfig logchannel` to see automod actions.".into());
    }
    problems
}

fn honeypot_permission(action: &str) -> (&'static str, &'static str) {
    match action {
        "kick" => ("KickMembers", "Kick Members"),
        "timeout" => ("ModerateMembers", "Timeout Members"),
        _ => ("BanMembers", "Ban Members"),
    }
}

fn parse_channel(id: &str) -> Option<ChannelId> {
    id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
}

fn channel_cached(app: &App, guild_id: GuildId, id: &str) -> bool {
    parse_channel(id).is_some_and(|c| bot_core::interaction_helpers::cached_channel(app, guild_id, c).is_some())
}

/// What would stop the honeypot from working: the punishment permission, and per channel that it still exists and
/// that I can see it (and delete in it, for kick and timeout; a ban clears the message itself).
pub fn collect_honeypot_problems(app: &App, guild_id: GuildId, config: &AutomodConfig) -> Vec<String> {
    let hp = &config.honeypot;
    if hp.channel_ids.is_empty() {
        return Vec::new();
    }
    let mut problems = Vec::new();
    let (perm, name) = honeypot_permission(&hp.action);
    if bot_missing(app, guild_id, perm) {
        problems.push(format!("I need **{name}** for the honeypot's action."));
    }
    let needs: &[&str] = if hp.action == "kick" || hp.action == "timeout" { &["ViewChannel", "ManageMessages"] } else { &["ViewChannel"] };
    for id in &hp.channel_ids {
        if !channel_cached(app, guild_id, id) {
            problems.push(format!("A honeypot channel (`{id}`) was deleted. It is forgotten the next time you use `/automod honeypot`."));
            continue;
        }
        let usable = check_channel_usable(app, guild_id, parse_channel(id), needs);
        if !usable.ok {
            problems.push(format!("Honeypot <#{id}>: {}", usable.reason.unwrap_or_default()));
        }
    }
    problems
}

fn problem_lines(problems: &[String]) -> String {
    problems.iter().map(|p| format!("⚠\u{FE0F} {p}")).collect::<Vec<_>>().join("\n")
}

async fn edit(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

fn gid(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/automod outside a guild"))
}

// ── enable / disable / status ─────────────────────────────────────────────────

async fn enable(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let settings = ctx
        .app
        .settings()
        .update_settings_with(&guild_id.to_string(), |cur| {
            Ok(SettingsPatch::new().set("automodEnabled", true).set("automodConfig", normalize_config(cur.automod_config.as_ref()).to_json()))
        })
        .await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    let active: Vec<&str> = RULE_KEYS.iter().copied().filter(|k| config.rule(k).action != "off").collect();
    let head = if active.is_empty() {
        "No rules are active yet. Turn one on with `/automod rule`.".to_string()
    } else {
        format!("Active rules:\n{}", active.iter().map(|k| describe_rule_line(k, &config.rule(k), config)).collect::<Vec<_>>().join("\n"))
    };
    // The head already says when no rule is on, so that warning is not repeated under it.
    let problems: Vec<String> = collect_problems(&ctx.app, guild_id, Some(&settings), config, None).into_iter().filter(|p| p != NO_RULES_PROBLEM).collect();
    let body = if problems.is_empty() { head } else { format!("{head}\n\n{}", problem_lines(&problems)) };
    edit(ctx, success_embed("Automod Enabled", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

async fn disable(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    ctx.app.settings().update_settings(&guild_id.to_string(), SettingsPatch::new().set("automodEnabled", false)).await?;
    edit(ctx, success_embed("Automod Disabled", Some("Automod is off. Your rules, lists and exemptions are kept."), None)).await;
    Ok(())
}

/// The `/automod status` embed: `(title, description, footer, fields)`.
pub fn status_parts(app: &App, guild_id: GuildId, settings: &GuildSettings, config: &AutomodConfig) -> (String, String, Option<String>, Vec<(String, String)>) {
    let description = [
        if settings.automod_enabled { "**Status:** ✅ On".to_string() } else { "**Status:** ❌ Off — turn it on with `/automod enable`.".to_string() },
        match settings.mod_log_channel_id.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => format!("**Mod log:** <#{c}>"),
            None => "**Mod log:** not set — use `/modconfig logchannel`.".to_string(),
        },
        "Staff always bypass automod: the server owner, admins, moderators, members with Manage Messages and immune roles (`/modconfig immune`).".to_string(),
    ]
    .join("\n");
    let title = "🤖 Auto-Moderation".to_string();
    let footer = is_legacy_config(settings.automod_config.as_ref()).then(|| "Stored in the old format; it is upgraded the next time you change a setting.".to_string());

    // Discord caps an embed at 6000 characters in total: fields are added while the budget lasts.
    let mut budget = 5800i64 - len16(&title) as i64 - len16(&description) as i64 - footer.as_deref().map(len16).unwrap_or(0) as i64;
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut add_field = |name: &str, value: &str| {
        let room = 1024i64.min(budget - len16(name) as i64);
        if room < 40 || value.is_empty() {
            return;
        }
        let text = clamp_text(value, room as usize);
        budget -= (len16(name) + len16(&text)) as i64;
        fields.push((name.to_string(), text));
    };

    let lines: Vec<String> = RULE_KEYS.iter().map(|k| describe_rule_line(k, &config.rule(k), config)).collect();
    for (i, chunk) in chunk_lines(&lines, 1024).iter().enumerate() {
        add_field(if i == 0 { "Rules" } else { "Rules (cont.)" }, chunk);
    }
    add_field(&format!("{HONEYPOT_EMOJI} {HONEYPOT_LABEL}"), &describe_honeypot_value(config));
    add_field(
        "Lists",
        &format!(
            "Bad words: {} · Patterns: {} · Blocked domains: {} · Allowed domains: {}\n{}",
            config.bad_words.len(),
            config.bad_word_patterns.len(),
            config.blocked_domains.len(),
            config.allowed_domains.len(),
            describe_links_mode(config)
        ),
    );
    add_field(
        "Exempt from all rules",
        &format!(
            "Roles: {}\nChannels: {}",
            if config.exempt_role_ids.is_empty() { "none".to_string() } else { role_mentions(&config.exempt_role_ids, 480) },
            if config.exempt_channel_ids.is_empty() { "none".to_string() } else { channel_mentions(&config.exempt_channel_ids, 480) }
        ),
    );
    let mut scope_lines = Vec::new();
    for k in RULE_KEYS {
        let r = config.rule(k);
        if r.role_ids.is_empty() && r.channel_ids.is_empty() && r.role_mode == "except" && r.channel_mode == "except" {
            continue;
        }
        scope_lines.push(format!(
            "{}: roles {} {}; channels {} {}",
            rule_meta(k).map(|m| m.label).unwrap_or(k),
            r.role_mode,
            if r.role_ids.is_empty() { "none".to_string() } else { role_mentions(&r.role_ids, 150) },
            r.channel_mode,
            if r.channel_ids.is_empty() { "none".to_string() } else { channel_mentions(&r.channel_ids, 150) }
        ));
    }
    for (i, chunk) in chunk_lines(&scope_lines, 1024).iter().enumerate() {
        add_field(if i == 0 { "Rule scopes" } else { "Rule scopes (cont.)" }, chunk);
    }
    let problems = collect_problems(app, guild_id, Some(settings), config, None);
    if !problems.is_empty() {
        add_field("⚠\u{FE0F} Problems", &problems.join("\n"));
    }
    (title, description, footer, fields)
}

async fn status(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let (title, description, footer, fields) = status_parts(&ctx.app, guild_id, &settings, &compiled.config);
    let mut embed = apply_guild_color(neutral_embed(Some(&title), Some(&description), footer.as_deref()), Some(&settings));
    for (name, value) in fields {
        embed = embed.field(name, value, false);
    }
    edit(ctx, embed).await;
    Ok(())
}

// ── rule ──────────────────────────────────────────────────────────────────────

async fn rule_set(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let Some(key) = resolve_rule_key(&ctx.get_string("rule").unwrap_or_default()) else {
        edit(ctx, unknown_rule()).await;
        return Ok(());
    };
    let action = ctx.get_string("action").ok_or_else(|| anyhow::anyhow!("Required option \"action\" not found."))?;
    let threshold = ctx.get_integer("threshold");
    let timeout = ctx.get_string("timeout");
    let roles = ctx.get_string("roles");
    let channels = ctx.get_string("channels");
    let Some(meta) = rule_meta(key) else { return Ok(()) };
    let invalid = |msg: String| error_embed("Invalid Option", Some(&msg), None);

    if threshold.is_some() && meta.threshold.is_none() {
        edit(ctx, invalid(format!("**{}** has no threshold.", meta.label))).await;
        return Ok(());
    }
    if let (Some(n), Some(t)) = (threshold, meta.threshold)
        && (n < t.min || n > t.max)
    {
        edit(ctx, invalid(format!("The threshold for **{}** must be between {} and {} ({}).", meta.label, t.min, t.max, t.unit))).await;
        return Ok(());
    }
    if timeout.is_some() && action != "timeout" {
        edit(ctx, invalid("The `timeout` option only applies to the **Delete & timeout** action.".into())).await;
        return Ok(());
    }
    let mut ms = None;
    if let Some(t) = &timeout {
        match bot_util::parse_duration::parse_duration(t).filter(|m| *m >= MIN_TIMEOUT_SECONDS * 1000 && *m <= MAX_TIMEOUT_SECONDS * 1000) {
            Some(m) => ms = Some(m),
            None => {
                edit(ctx, invalid("Timeout must be between 1 minute and 28 days, e.g. `10m`, `2h` or `1d`.".into())).await;
                return Ok(());
            }
        }
    }

    let a = action.clone();
    let (settings, ()) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, _: &mut ()| {
        let r = cfg.rule_mut(key);
        r.action = a;
        if let Some(n) = threshold {
            r.threshold = Some(n);
        }
        if let Some(m) = ms {
            r.timeout_seconds = m / 1000;
        }
        if let Some(m) = roles.filter(|m| !m.is_empty()) {
            r.role_mode = m;
        }
        if let Some(m) = channels.filter(|m| !m.is_empty()) {
            r.channel_mode = m;
        }
        true
    })
    .await?;

    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    let r = config.rule(key);
    let mut lines = vec![describe_rule_line(key, &r, config)];
    lines.push(if r.role_mode == "only" {
        format!("Roles: only {}", if r.role_ids.is_empty() { "none listed (applies to nobody)".to_string() } else { role_mentions(&r.role_ids, 300) })
    } else {
        format!("Roles: everyone{}", if r.role_ids.is_empty() { String::new() } else { format!(" except {}", role_mentions(&r.role_ids, 300)) })
    });
    lines.push(if r.channel_mode == "only" {
        format!("Channels: only {}", if r.channel_ids.is_empty() { "none listed (applies to nobody)".to_string() } else { channel_mentions(&r.channel_ids, 300) })
    } else {
        format!("Channels: every channel{}", if r.channel_ids.is_empty() { String::new() } else { format!(" except {}", channel_mentions(&r.channel_ids, 300)) })
    });
    if r.action == "off" {
        lines.push(format!("**{}** is now off.", meta.label));
    }
    let mut problems = collect_problems(&ctx.app, guild_id, Some(&settings), config, Some(key));
    if !settings.automod_enabled {
        problems.push("Automod is off — turn it on with `/automod enable`.".into());
    }
    let body = if problems.is_empty() { lines.join("\n") } else { format!("{}\n\n{}", lines.join("\n"), problem_lines(&problems)) };
    edit(ctx, success_embed("Rule Updated", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

// ── words (ephemeral) ─────────────────────────────────────────────────────────

/// A listed word as a spoiler. Markdown in it (the whole-word `_word_` form) would otherwise turn into italics.
pub fn spoilered(w: &str) -> String {
    format!("||{}||", escape_markdown(w))
}

#[derive(Default, Clone)]
struct WordsReport {
    added: Vec<String>,
    skipped: Vec<String>,
}

/// `wordsAdd`'s parse: `(valid words, invalid notes)`.
pub fn parse_word_list(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for part in raw.split([',', '\n']) {
        let nf: String = unicode_normalization::UnicodeNormalization::nfkc(part).collect();
        let cleaned = bot_util::js::lower(bot_util::js::trim(&strip_invisible(&nf)));
        let cleaned = {
            let mut out = String::new();
            let mut sp = false;
            for c in cleaned.chars() {
                if bot_util::js::is_js_space(c) {
                    if !sp {
                        out.push(' ');
                    }
                    sp = true;
                } else {
                    out.push(c);
                    sp = false;
                }
            }
            out
        };
        if cleaned.is_empty() {
            continue;
        }
        if let Some(w) = normalize_word(&cleaned) {
            valid.push(w);
            continue;
        }
        let shown = escape_markdown(&clamp_text(&cleaned, 40));
        let why = if len16(&cleaned) < 2 {
            "too short"
        } else if len16(&cleaned) > 64 {
            "too long"
        } else {
            "no letters or digits"
        };
        invalid.push(format!("{shown} ({why})"));
    }
    (valid, invalid)
}

async fn words_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let raw = ctx.get_string("words").unwrap_or_default();
    let (valid, invalid) = parse_word_list(&raw);
    let (settings, report) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut WordsReport| {
        for w in valid {
            if cfg.bad_words.contains(&w) {
                rep.skipped.push(format!("{} (already listed)", escape_markdown(&w)));
            } else if cfg.bad_words.len() >= bot_config::limits::AUTOMOD_WORDS {
                rep.skipped.push(format!("{} (list full: {})", escape_markdown(&w), bot_config::limits::AUTOMOD_WORDS));
            } else {
                cfg.bad_words.push(w.clone());
                rep.added.push(w);
            }
        }
        !rep.added.is_empty()
    })
    .await?;

    let notes: Vec<String> = report.skipped.iter().cloned().chain(invalid).collect();
    if report.added.is_empty() {
        let text = if notes.is_empty() { "No words were given.".to_string() } else { format!("Skipped: {}", notes.join(", ")) };
        edit(ctx, warn_embed("Nothing Added", Some(&clamp_text(&text, 4096)), None)).await;
        return Ok(());
    }
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let mut body = clamp_text(&format!("Added **{}**: {}", report.added.len(), report.added.iter().map(|w| spoilered(w)).collect::<Vec<_>>().join(", ")), 3500);
    if !notes.is_empty() {
        body.push_str(&format!("\nSkipped: {}", clamp_text(&notes.join(", "), 400)));
    }
    body.push_str(&off_hint("badWords", &compiled.config));
    edit(ctx, success_embed("Words Added", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

async fn words_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let w = normalize_word(&ctx.get_string("word").unwrap_or_default());
    let target = w.clone();
    let (_, not_found) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut bool| {
        let idx = target.as_ref().and_then(|t| cfg.bad_words.iter().position(|x| x == t));
        match idx {
            None => {
                *rep = true;
                false
            }
            Some(i) => {
                cfg.bad_words.remove(i);
                true
            }
        }
    })
    .await?;
    match w {
        Some(w) if !not_found => edit(ctx, success_embed("Word Removed", Some(&format!("{} is no longer filtered.", spoilered(&w))), None)).await,
        _ => edit(ctx, warn_embed("Not Listed", Some("That word is not on the list."), None)).await,
    }
    Ok(())
}

async fn send_pages(ctx: &CommandCtx, pages: Vec<CreateEmbed>) -> anyhow::Result<()> {
    if pages.len() == 1 {
        safe_reply(&ctx.responder, Payload::embed(pages[0].clone())).await;
        return Ok(());
    }
    paginate(ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

async fn words_list(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    if config.bad_words.is_empty() {
        edit(ctx, info_embed("Bad Words", Some("The list is empty. Add words with `/automod words add`."), None)).await;
        return Ok(());
    }
    let items: Vec<String> = config.bad_words.iter().map(|w| spoilered(w)).collect();
    let title = format!("🤬 Bad Words ({})", config.bad_words.len());
    let pages = pack_items(&items, ", ", 3900).iter().map(|t| neutral_embed(Some(&title), Some(t), None)).collect();
    send_pages(ctx, pages).await
}

// ── patterns (ephemeral) ──────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct PatternReport {
    duplicate: bool,
    full: bool,
    index: usize,
}

async fn patterns_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let p = bot_util::js::trim(&ctx.get_string("pattern").unwrap_or_default()).to_string();
    if let Err(reason) = validate_regex(&p, "i") {
        edit(ctx, error_embed("Invalid Pattern", Some(&reason), None)).await;
        return Ok(());
    }
    let Some(re) = compile_safe_regex(&p, "i") else {
        edit(ctx, error_embed("Invalid Pattern", Some("This pattern could not be compiled."), None)).await;
        return Ok(());
    };
    if safe_regex_test(&re, "", None) || safe_regex_test(&re, " ", None) {
        edit(ctx, error_embed("Invalid Pattern", Some("This pattern matches empty text, so it would flag every message."), None)).await;
        return Ok(());
    }
    let pat = p.clone();
    let (settings, report) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut PatternReport| {
        if cfg.bad_word_patterns.contains(&pat) {
            rep.duplicate = true;
            return false;
        }
        if cfg.bad_word_patterns.len() >= bot_config::limits::AUTOMOD_PATTERNS {
            rep.full = true;
            return false;
        }
        cfg.bad_word_patterns.push(pat);
        rep.index = cfg.bad_word_patterns.len();
        true
    })
    .await?;
    if report.duplicate {
        edit(ctx, warn_embed("Already Listed", Some("That pattern is already on the list."), None)).await;
        return Ok(());
    }
    if report.full {
        edit(ctx, error_embed("List Full", Some(&format!("You can have at most {} patterns.", bot_config::limits::AUTOMOD_PATTERNS)), None)).await;
        return Ok(());
    }
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let body = format!("Pattern **#{}** saved: `{}`{}", report.index, clamp_text(&p, 200), off_hint("badWords", &compiled.config));
    edit(ctx, success_embed("Pattern Added", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

async fn patterns_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let option = bot_util::js::trim(&ctx.get_string("pattern").unwrap_or_default()).to_string();
    let (_, removed) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut Option<String>| {
        let list = &mut cfg.bad_word_patterns;
        let mut idx = None;
        if (1..=3).contains(&option.len()) && option.bytes().all(|b| b.is_ascii_digit()) {
            let i = option.parse::<usize>().unwrap_or(0);
            if i >= 1 && i <= list.len() {
                idx = Some(i - 1);
            }
        }
        let idx = idx.or_else(|| list.iter().position(|x| *x == option));
        match idx {
            None => false,
            Some(i) => {
                *rep = Some(list.remove(i));
                true
            }
        }
    })
    .await?;
    match removed {
        None => edit(ctx, warn_embed("Not Listed", Some("No pattern matches that. Pick one from the list."), None)).await,
        Some(p) => edit(ctx, success_embed("Pattern Removed", Some(&format!("`{}` removed.", clamp_text(&p, 200))), None)).await,
    }
    Ok(())
}

async fn patterns_list(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    if config.bad_word_patterns.is_empty() {
        edit(ctx, info_embed("Regex Patterns", Some("No patterns yet. Add one with `/automod patterns add`."), None)).await;
        return Ok(());
    }
    let lines: Vec<String> = config.bad_word_patterns.iter().enumerate().map(|(i, p)| format!("**#{}** `{}`", i + 1, p.replace('`', "ˋ"))).collect();
    let title = format!("🧩 Regex Patterns ({})", config.bad_word_patterns.len());
    let pages = chunk_lines(&lines, 3900).iter().map(|t| neutral_embed(Some(&title), Some(t), None)).collect();
    send_pages(ctx, pages).await
}

// ── domains ───────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct DomainReport {
    duplicate: bool,
    full: bool,
    moved: bool,
}

fn domain_list<'a>(cfg: &'a mut AutomodConfig, name: &str) -> &'a mut Vec<String> {
    if name == "blocked" { &mut cfg.blocked_domains } else { &mut cfg.allowed_domains }
}

async fn domains_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let Some(d) = normalize_domain(&ctx.get_string("domain").unwrap_or_default()) else {
        edit(ctx, error_embed("Invalid Domain", Some("That doesn't look like a domain. Use something like `example.com`."), None)).await;
        return Ok(());
    };
    let list = ctx.get_string("list").unwrap_or_else(|| "blocked".into());
    let other = if list == "blocked" { "allowed" } else { "blocked" };
    let (dd, l) = (d.clone(), list.clone());
    let (settings, report) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut DomainReport| {
        let target_len = domain_list(cfg, &l).len();
        if domain_list(cfg, &l).contains(&dd) {
            rep.duplicate = true;
            return false;
        }
        if target_len >= bot_config::limits::AUTOMOD_DOMAINS {
            rep.full = true;
            return false;
        }
        let opposite = domain_list(cfg, other);
        if let Some(at) = opposite.iter().position(|x| *x == dd) {
            opposite.remove(at);
            rep.moved = true;
        }
        domain_list(cfg, &l).push(dd);
        true
    })
    .await?;
    if report.duplicate {
        edit(ctx, warn_embed("Already Listed", Some(&format!("**{d}** is already on the {list} list.")), None)).await;
        return Ok(());
    }
    if report.full {
        edit(ctx, error_embed("List Full", Some(&format!("Each domain list holds at most {} domains.", bot_config::limits::AUTOMOD_DOMAINS)), None)).await;
        return Ok(());
    }
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let mut body = format!("**{d}** (and its subdomains) is now **{list}**.");
    if report.moved {
        body.push_str(&format!("\nIt was removed from the {other} list."));
    }
    body.push_str(&format!("\n{}{}", describe_links_mode(&compiled.config), off_hint("links", &compiled.config)));
    edit(ctx, success_embed("Domain Added", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

async fn domains_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let input = ctx.get_string("domain").unwrap_or_default();
    let d = normalize_domain(&input).unwrap_or_else(|| bot_util::js::lower(bot_util::js::trim(&input)));
    let dd = d.clone();
    let (_, from) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut Vec<&'static str>| {
        for name in ["blocked", "allowed"] {
            let l = domain_list(cfg, name);
            if let Some(at) = l.iter().position(|x| *x == dd) {
                l.remove(at);
                rep.push(name);
            }
        }
        !rep.is_empty()
    })
    .await?;
    if from.is_empty() {
        edit(ctx, warn_embed("Not Listed", Some("That domain is on neither list."), None)).await;
    } else {
        edit(ctx, success_embed("Domain Removed", Some(&format!("**{d}** removed from the {} list.", from.join(" and "))), None)).await;
    }
    Ok(())
}

async fn domains_list(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    let list = |l: &[String]| if l.is_empty() { "none".to_string() } else { join_with_limit(l, ", ", 1024, default_more) };
    let embed = apply_guild_color(neutral_embed(Some("🔗 External Links — Domains"), Some(describe_links_mode(config)), None), Some(&settings))
        .field("Blocked", list(&config.blocked_domains), false)
        .field("Allowed", list(&config.allowed_domains), false);
    edit(ctx, embed).await;
    Ok(())
}

// ── exempt ────────────────────────────────────────────────────────────────────

async fn exempt_change(ctx: &CommandCtx, add: bool) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let role = ctx.get_role_id("role").map(|r| r.to_string());
    let channel = ctx.get_channel_id("channel").map(|c| c.to_string());
    let is_category = ctx.get_channel("channel").is_some_and(|c| c.kind == ChannelType::Category);
    let rule_opt = ctx.get_string("rule");
    if role.is_none() && channel.is_none() {
        edit(ctx, error_embed("Nothing Selected", Some("Pick a role, a channel, or both."), None)).await;
        return Ok(());
    }
    if role.as_deref() == Some(guild_id.to_string().as_str()) {
        edit(ctx, error_embed("Invalid Role", Some("@everyone cannot be used here. Use /automod disable or a rule's action instead."), None)).await;
        return Ok(());
    }
    let mut key: Option<&'static str> = None;
    if let Some(r) = rule_opt.filter(|r| !r.is_empty()) {
        match resolve_rule_key(&r) {
            Some(k) => key = Some(k),
            None => {
                edit(ctx, unknown_rule()).await;
                return Ok(());
            }
        }
    }
    let mut items: Vec<(bool, String, String)> = Vec::new(); // (is role, id, text)
    if let Some(r) = &role {
        items.push((true, r.clone(), format!("<@&{r}>")));
    }
    if let Some(c) = &channel {
        items.push((false, c.clone(), format!("<#{c}>")));
    }

    let (settings, lines) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut Vec<String>| {
        let mut changed = false;
        for (is_role, id, text) in &items {
            let list = match (key, is_role) {
                (Some(k), true) => &mut cfg.rule_mut(k).role_ids,
                (Some(k), false) => &mut cfg.rule_mut(k).channel_ids,
                (None, true) => &mut cfg.exempt_role_ids,
                (None, false) => &mut cfg.exempt_channel_ids,
            };
            let at = list.iter().position(|x| x == id);
            if add {
                if at.is_some() {
                    rep.push(format!("{text} was already listed"));
                } else if list.len() >= EXEMPT_CAP {
                    rep.push(format!("{text} was not added: the list is full ({EXEMPT_CAP})"));
                } else {
                    list.push(id.clone());
                    rep.push(format!("Added {text}"));
                    changed = true;
                }
            } else if let Some(i) = at {
                list.remove(i);
                rep.push(format!("Removed {text}"));
                changed = true;
            } else {
                rep.push(format!("{text} was not listed"));
            }
        }
        changed
    })
    .await?;

    let mut lines = lines;
    let changed = lines.iter().any(|l| l.starts_with("Added ") || l.starts_with("Removed "));
    if !changed {
        edit(ctx, warn_embed("No Change", Some(&clamp_text(&lines.join("\n"), 4096)), None)).await;
        return Ok(());
    }
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    let (roles, channels) = match key {
        Some(k) => (config.rule(k).role_ids, config.rule(k).channel_ids),
        None => (config.exempt_role_ids.clone(), config.exempt_channel_ids.clone()),
    };
    let roles_text = if roles.is_empty() { "none".to_string() } else { role_mentions(&roles, 700) };
    let channels_text = if channels.is_empty() { "none".to_string() } else { channel_mentions(&channels, 700) };
    match key {
        None => lines.push(format!("Exempt from every rule: roles: {roles_text}; channels: {channels_text}")),
        Some(k) => {
            let r = config.rule(k);
            let label = rule_meta(k).map(|m| m.label).unwrap_or(k);
            if r.role_mode == "except" && r.channel_mode == "except" {
                lines.push(format!("**{label}** ignores: roles: {roles_text}; channels: {channels_text}"));
            } else if r.role_mode == "only" && r.channel_mode == "only" {
                lines.push(format!("**{label}** applies only to: roles: {roles_text}; channels: {channels_text}"));
            } else {
                lines.push(format!("**{label}** scope: roles {} {roles_text}; channels {} {channels_text}", r.role_mode, r.channel_mode));
            }
        }
    }
    if add && is_category {
        lines.push("Every channel and thread inside this category is covered.".into());
    }
    edit(ctx, success_embed("Exemptions Updated", Some(&clamp_text(&lines.join("\n"), 4096)), None)).await;
    Ok(())
}

// ── honeypot ─────────────────────────────────────────────────────────────────

fn honeypot_outcome_text(action: &str) -> &'static str {
    match action {
        "ban" => "banned, and their messages from the last day are deleted",
        "kick" => "kicked",
        "timeout" => "timed out",
        _ => "removed from the server, and their messages from the last day are deleted",
    }
}

/// The warning posted in a honeypot channel.
pub fn honeypot_warning_embed(action: &str) -> CreateEmbed {
    warn_embed(
        &format!("{HONEYPOT_EMOJI} Do not post here"),
        Some(&format!(
            "This channel is a trap for spam bots and hacked accounts.\n\n**Anyone who posts here is automatically {}.**\n\nIf you are a real person, just ignore this channel.",
            honeypot_outcome_text(action)
        )),
        None,
    )
}

fn honeypot_full() -> CreateEmbed {
    error_embed("Too Many Honeypots", Some(&format!("You already have {HONEYPOT_CAP} honeypot channels. Remove one with `/automod honeypot remove` first.")), None)
}

/// The honeypot ids of a config whose channels still exist.
fn existing_ids(app: &App, guild_id: GuildId, ids: &[String]) -> Vec<String> {
    ids.iter().filter(|id| channel_cached(app, guild_id, id)).cloned().collect()
}

#[derive(Default, Clone)]
struct HoneypotReport {
    full: bool,
}

async fn honeypot_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let action = ctx.get_string("action");
    let timeout = ctx.get_string("timeout");
    let post_warning = ctx.get_boolean("warning").unwrap_or(true);
    let invalid = |msg: &str| error_embed("Invalid Option", Some(msg), None);
    if timeout.is_some() && action.as_deref() != Some("timeout") {
        edit(ctx, invalid("The `timeout` option only applies with `action:` **Delete & timeout**.")).await;
        return Ok(());
    }
    let mut ms = None;
    if let Some(t) = &timeout {
        match bot_util::parse_duration::parse_duration(t).filter(|m| *m >= MIN_TIMEOUT_SECONDS * 1000 && *m <= MAX_TIMEOUT_SECONDS * 1000) {
            Some(m) => ms = Some(m),
            None => {
                edit(ctx, invalid("Timeout must be between 1 minute and 28 days, e.g. `1h` or `1d`.")).await;
                return Ok(());
            }
        }
    }

    let mut channel = ctx.get_channel_id("channel");
    let mut created = false;
    if channel.is_none() {
        let settings = ctx.app.settings().get_settings(&g).await?;
        let compiled = get_automod_config(&ctx.app, Some(&settings));
        if existing_ids(&ctx.app, guild_id, &compiled.config.honeypot.channel_ids).len() >= HONEYPOT_CAP {
            edit(ctx, honeypot_full()).await;
            return Ok(());
        }
        if bot_missing(&ctx.app, guild_id, "ManageChannels") {
            edit(
                ctx,
                error_embed(
                    "Missing Permission",
                    Some("I need **Manage Channels** to create the honeypot channel. Give me the permission, or pick an existing channel."),
                    None,
                ),
            )
            .await;
            return Ok(());
        }
        let reason = clamp_text(&format!("Honeypot channel created with /automod honeypot add by {}", bot_core::adapters::user_tag(ctx.user())), 512);
        let builder = CreateChannel::new(HONEYPOT_CHANNEL_NAME)
            .kind(ChannelType::Text)
            .position(0)
            .topic("Do not post here. This channel is a trap for spam bots and hacked accounts.")
            .audit_log_reason(&reason);
        let result = match ctx.app.http() {
            Some(http) => guild_id.create_channel(http.as_ref(), builder).await.map_err(|e| bot_core::discord::error_message(&e)),
            None => Err("offline".to_string()),
        };
        match result {
            Ok(c) => {
                bot_core::discord::cache_created_channel(&ctx.app, &c);
                channel = Some(c.id);
                created = true;
            }
            Err(e) => {
                bot_util::log_warn!(format!("Could not create a honeypot channel in {g}: {e}"));
                edit(
                    ctx,
                    error_embed(
                        "Could Not Create Channel",
                        Some("Discord refused to create the channel. Check my **Manage Channels** permission, or pick an existing channel."),
                        None,
                    ),
                )
                .await;
                return Ok(());
            }
        }
    }
    let Some(channel) = channel else { return Ok(()) };
    let cid = channel.to_string();

    let app = ctx.app.clone();
    let (c2, act) = (cid.clone(), action.clone());
    let (settings, report) = write_config(&ctx.app, &g, move |cfg, rep: &mut HoneypotReport| {
        // forgetDeleted: drop honeypot channels that were deleted.
        cfg.honeypot.channel_ids.retain(|id| channel_cached(&app, guild_id, id));
        let hp = &mut cfg.honeypot;
        if !hp.channel_ids.contains(&c2) {
            if hp.channel_ids.len() >= HONEYPOT_CAP {
                rep.full = true;
                return false;
            }
            hp.channel_ids.push(c2);
        }
        if let Some(a) = act.filter(|a| !a.is_empty()) {
            hp.action = a;
        }
        if let Some(m) = ms {
            hp.timeout_seconds = m / 1000;
        }
        true
    })
    .await?;
    if report.full {
        if created && let Some(http) = ctx.app.http() {
            let _ = http.delete_channel(channel, Some("Honeypot limit reached")).await;
        }
        edit(ctx, honeypot_full()).await;
        return Ok(());
    }

    let compiled = get_automod_config(&ctx.app, Some(&settings));
    let config = &compiled.config;
    let hp = &config.honeypot;
    let mut problems = Vec::new();
    let mut warned = false;
    if post_warning {
        // The picked channel object (discord.js resolves it from the interaction even when it is not cached).
        let usable = bot_core::interaction_helpers::check_channel_usable_fetch(&ctx.app, guild_id, Some(channel), &["ViewChannel", "SendMessages", "EmbedLinks"]).await;
        if !usable.ok {
            problems.push(format!("The warning was not posted: {}", usable.reason.unwrap_or_default()));
        } else {
            let sent = match ctx.app.http() {
                Some(http) => channel
                    .send_message(http.as_ref(), CreateMessage::new().embed(honeypot_warning_embed(&hp.action)).allowed_mentions(CreateAllowedMentions::new()))
                    .await
                    .map_err(|e| bot_core::discord::error_message(&e)),
                None => Err("offline".to_string()),
            };
            match sent {
                Ok(_) => warned = true,
                Err(e) => {
                    problems.push("The warning could not be posted in the channel.".to_string());
                    bot_util::log_warn!(format!("Could not post the honeypot warning in {g}: {e}"));
                }
            }
        }
    }
    problems.extend(collect_honeypot_problems(&ctx.app, guild_id, config));
    if !settings.automod_enabled {
        problems.push("Automod is off, so the honeypot does nothing yet. Turn it on with `/automod enable`.".into());
    }
    let timeout_text = if hp.action == "timeout" { format!(" for {}", bot_util::parse_duration::format_duration_long(hp.timeout_seconds as f64 * 1000.0)) } else { String::new() };
    let lines = [
        format!(
            "<#{cid}> is now a honeypot.{}{}",
            if created { " I created it at the top of the channel list." } else { "" },
            if warned { " A warning is posted in it." } else { "" }
        ),
        format!("Whoever posts there: **{}**{timeout_text}.", honeypot_action_label(&hp.action).unwrap_or(&hp.action)),
        "The owner, staff (moderators, Manage Messages, immune roles) and exempt roles are never caught, so staff can still post there.".to_string(),
        "Keep it near the top and visible to everyone: spam bots post in the first channels they can see.".to_string(),
        String::new(),
        describe_honeypot(config),
    ];
    let body = if problems.is_empty() { lines.join("\n") } else { format!("{}\n\n{}", lines.join("\n"), problem_lines(&problems)) };
    edit(ctx, success_embed("Honeypot Set", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

async fn honeypot_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    let guild_id = gid(ctx)?;
    let channel = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let cid = channel.to_string();
    let app = ctx.app.clone();
    let c2 = cid.clone();
    let (settings, was) = write_config(&ctx.app, &guild_id.to_string(), move |cfg, rep: &mut bool| {
        let before = cfg.honeypot.channel_ids.len();
        *rep = cfg.honeypot.channel_ids.contains(&c2);
        cfg.honeypot.channel_ids.retain(|id| channel_cached(&app, guild_id, id));
        cfg.honeypot.channel_ids.retain(|id| *id != c2);
        cfg.honeypot.channel_ids.len() != before
    })
    .await?;
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    if !was {
        edit(ctx, info_embed("Not a Honeypot", Some(&format!("<#{cid}> is not a honeypot channel.\n\n{}", describe_honeypot(&compiled.config))), None)).await;
        return Ok(());
    }
    let body = format!(
        "<#{cid}> is no longer a honeypot. The channel and its warning are kept; delete them if you no longer need them.\n\n{}",
        describe_honeypot(&compiled.config)
    );
    edit(ctx, success_embed("Honeypot Removed", Some(&clamp_text(&body, 4096)), None)).await;
    Ok(())
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    // Replies for words and patterns echo slurs and filter-evasion patterns: they are private.
    let ephemeral = matches!(group.as_deref(), Some("words") | Some("patterns"));
    ctx.defer_reply(ephemeral).await?;

    let name = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    let result = match name.as_str() {
        "enable" => enable(&ctx).await,
        "disable" => disable(&ctx).await,
        "status" => status(&ctx).await,
        "rule" => rule_set(&ctx).await,
        "words add" => words_add(&ctx).await,
        "words remove" => words_remove(&ctx).await,
        "words list" => words_list(&ctx).await,
        "patterns add" => patterns_add(&ctx).await,
        "patterns remove" => patterns_remove(&ctx).await,
        "patterns list" => patterns_list(&ctx).await,
        "domains add" => domains_add(&ctx).await,
        "domains remove" => domains_remove(&ctx).await,
        "domains list" => domains_list(&ctx).await,
        "exempt add" => exempt_change(&ctx, true).await,
        "exempt remove" => exempt_change(&ctx, false).await,
        "honeypot add" => honeypot_add(&ctx).await,
        "honeypot remove" => honeypot_remove(&ctx).await,
        _ => {
            edit(&ctx, error_embed("Unknown Command", Some("That subcommand does not exist."), None)).await;
            return Ok(());
        }
    };
    if let Err(e) = result {
        bot_util::log_error!("automod command failed".to_string(), bot_core::error::log_arg(&e));
        let read_only = sub == "status" || sub == "list";
        let _ = ctx.edit_reply(Payload::embed(if read_only { settings_unavailable() } else { save_failed() }).to_edit()).await;
    }
    Ok(())
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/// The `rule` choices for typed text `q` (lower-cased), with the current action when the settings are memoised.
pub fn rule_choices(q: &str, config: Option<&AutomodConfig>) -> Vec<Choice> {
    svc::RULES
        .iter()
        .filter(|m| bot_util::js::lower(m.key).contains(q) || bot_util::js::lower(m.label).contains(q))
        .map(|m| {
            let action = config.map(|c| format!(" — {}", action_label(&c.rule(m.key).action).unwrap_or_default())).unwrap_or_default();
            Choice::new(format!("{} {} ({}){action}", m.emoji, m.label, m.key), m.key)
        })
        .collect()
}

/// The choices of the other autocomplete fields (`word`, `pattern`, `domain`).
pub fn list_choices(field: &str, q: &str, config: &AutomodConfig) -> Vec<Choice> {
    match field {
        "word" => config.bad_words.iter().filter(|w| w.contains(q)).take(25).map(|w| Choice::new(w.clone(), w.clone())).collect(),
        "pattern" => {
            let mut out = Vec::new();
            for (i, p) in config.bad_word_patterns.iter().enumerate() {
                if out.len() < 25 && (bot_util::js::lower(p).contains(q) || (i + 1).to_string().starts_with(q)) {
                    out.push(Choice::new(clamp_text(&format!("#{} {p}", i + 1), 100), (i + 1).to_string()));
                }
            }
            out
        }
        "domain" => config
            .blocked_domains
            .iter()
            .map(|d| (d, "blocked"))
            .chain(config.allowed_domains.iter().map(|d| (d, "allowed")))
            .filter(|(d, _)| d.contains(q))
            .take(25)
            .map(|(d, kind)| Choice::new(format!("{d} ({kind})"), d.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let Some((name, value)) = ctx.focused() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let q = bot_util::js::lower(&value);
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    if name == "rule" {
        let settings = ctx.app.settings().peek_settings(&g); // sync only, no I/O
        let compiled = settings.as_deref().map(|s| get_automod_config(&ctx.app, Some(s)));
        respond_autocomplete(&ctx.responder, &rule_choices(&q, compiled.as_ref().map(|c| &c.config))).await;
        return Ok(());
    }
    let Some(settings) = ctx.app.settings().peek_or_load(&g, ctx.ack_budget_ms()).await else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let compiled = get_automod_config(&ctx.app, Some(&settings));
    respond_autocomplete(&ctx.responder, &list_choices(&name, &q, &compiled.config)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rule_names_reads_like_the_js() {
        assert_eq!(
            super::rule_names(),
            "Bad Words, Repeated Text, Server Invites, External Links, Excessive Caps, Excessive Emojis, Excessive Spoilers, Excessive Mentions, Zalgo or Anti-Spam"
        );
    }
}
