//! Port of `src/commands/moderation/modconfig.js`: `/modconfig` (escalation tiers, immune and moderator roles, the mute
//! role, the case log, reply visibility, DMs, show). An ADMIN config command: public replies, works while the module
//! is off (`gated = false`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bot_core::interaction_helpers::{Choice, PermTarget, Payload, missing_bot_permissions, respond_autocomplete, safe_defer};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::prelude::*;
use bot_db::{GuildSettings, ModVisibility};
use bot_util::text::{chunk_lines, clamp_text, join_with_limit, default_more};
use bot_util::time::discord_timestamp;
use serde_json::json;
use serenity::all::{ChannelId, GuildChannel, GuildId, PermissionOverwriteType, Permissions, RoleId};
use serenity::builder::CreateEmbed;

use crate::guild_ops::{load_guild, rest, snowflake};
use crate::moderation_service::{Tier, count_overdue_bans, describe_tier, normalize_escalation_tiers};

/// `ACTION_CHOICES`.
pub const ACTION_CHOICES: [(&str, &str); 5] =
    [("Timeout", "TIMEOUT"), ("Mute (mute role)", "MUTE"), ("Kick", "KICK"), ("Temporary ban", "TEMPBAN"), ("Permanent ban", "BAN")];

/// 28 days in seconds.
pub const TIMEOUT_MAX_S: i64 = 2_419_200;
/// 1 year in seconds.
pub const YEAR_S: i64 = 31_536_000;

/// The command's data.
pub fn data() -> SlashCommand {
    let mut action = CommandOption::string("action", "Action to take").required(true);
    for (name, value) in ACTION_CHOICES {
        action = action.choice(name, value);
    }
    SlashCommand::new("modconfig", "Moderator settings: escalation, immune and mod roles, mute role, case log, visibility.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            CommandOption::sub_command_group("escalation", "Automatic actions after N warnings (MEE6 automated actions).")
                .option(
                    CommandOption::sub_command("add", "Add or update a tier: N warnings within a period, then an action.")
                        .option(CommandOption::integer("warns", "Active warnings needed to trigger it").required(true).min_value(1).max_value(100))
                        .option(action)
                        .option(CommandOption::string("within", "Only count warnings from this period, e.g. 7d (omit = no limit)").max_length(32))
                        .option(CommandOption::string("duration", "Action length, e.g. 1h or 3d (Timeout and Temporary ban need one)").max_length(32)),
                )
                .option(
                    CommandOption::sub_command("remove", "Remove an escalation tier.")
                        .option(CommandOption::integer("tier", "Tier to remove").required(true).min_value(1).autocomplete(true)),
                )
                .option(CommandOption::sub_command("list", "List the escalation tiers.")),
        )
        .option(
            CommandOption::sub_command_group("immune", "Roles the bot and automod won't moderate.")
                .option(CommandOption::sub_command("add", "Make a role immune.").option(CommandOption::role("role", "Role to make immune").required(true)))
                .option(CommandOption::sub_command("remove", "Remove a role from the immune list.").option(CommandOption::role("role", "Role to remove").required(true)))
                .option(CommandOption::sub_command("list", "List the immune roles.")),
        )
        .option(
            CommandOption::sub_command_group("modroles", "Roles that can use moderation commands.")
                .option(
                    CommandOption::sub_command("add", "Give a role moderator access.")
                        .option(CommandOption::role("role", "Moderator role"))
                        .option(new_role_option("role")),
                )
                .option(CommandOption::sub_command("remove", "Remove a moderator role.").option(CommandOption::role("role", "Role to remove").required(true)))
                .option(CommandOption::sub_command("list", "List the moderator roles.")),
        )
        .option(
            CommandOption::sub_command("muterole", "Set, create or clear the mute role (used for mutes over 28 days).")
                .option(CommandOption::role("role", "Existing role to use as the mute role"))
                .option(new_role_option("role"))
                .option(CommandOption::boolean("setup", "Create the role if needed and deny talking in every channel"))
                .option(CommandOption::boolean("clear", "Clear the mute role")),
        )
        .option(
            CommandOption::sub_command("logchannel", "Set or clear the channel for moderation case logs.")
                .option(
                    CommandOption::channel("channel", "Case log channel (omit to clear)")
                        .channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]),
                )
                .option(new_channel_option("channel", "channel")),
        )
        .option(
            CommandOption::sub_command("visibility", "Who sees moderation command replies (MEE6 command visibility).").option(
                CommandOption::string("mode", "Visibility mode")
                    .required(true)
                    .choice("Public: everyone sees the action", "PUBLIC")
                    .choice("Anonymous: public notice, moderator hidden", "ANONYMOUS")
                    .choice("Private: only the moderator sees it", "PRIVATE"),
            ),
        )
        .option(
            CommandOption::sub_command("dm", "Turn DMs to punished members on or off.")
                .option(CommandOption::boolean("enabled", "DM members when warned, muted, kicked or banned").required(true)),
        )
        .option(CommandOption::sub_command("show", "Show the current moderation settings."))
}

/// `/modconfig` (`module 'moderation'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// `ModConfigError`: a message that is safe to show (thrown inside a settings mutator or before a write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModConfigError(pub String);

impl std::fmt::Display for ModConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModConfigError {}

fn refuse(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ModConfigError(msg.into()))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// `rolesLine(ids)`.
pub fn roles_line(ids: &[String]) -> String {
    if ids.is_empty() {
        return "None".into();
    }
    let mentions: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
    join_with_limit(&mentions, ", ", 1024, default_more)
}

/// `tierLine(t)`: describeTier plus "(counts warnings since ...)".
pub fn tier_line(t: &Tier) -> String {
    let mut line = describe_tier(t);
    if let Some(since) = t.count_from.as_deref().and_then(bot_db::parse_js_date) {
        line.push_str(&format!(" (counts warnings since {})", discord_timestamp(&since, "d")));
    }
    line
}

/// `tierListText(tiers)`.
pub fn tier_list_text(tiers: &[Tier]) -> String {
    if tiers.is_empty() {
        "No escalation tiers. Add one with `/modconfig escalation add`.".into()
    } else {
        tiers.iter().map(tier_line).collect::<Vec<_>>().join("\n")
    }
}

/// `parseSeconds(str)`: whole seconds of a duration string.
pub fn parse_seconds(s: &str) -> Option<i64> {
    bot_util::parse_duration::parse_duration(s).map(|ms| bot_util::text::js_round(ms as f64 / 1000.0) as i64)
}

fn tiers_value(tiers: &[Tier]) -> serde_json::Value {
    serde_json::to_value(tiers).unwrap_or_else(|_| json!([]))
}

/// The validation of `/modconfig escalation add` before the write: `(withinSeconds, durationSeconds)`.
pub fn validate_tier(action: &str, within: Option<&str>, duration: Option<&str>) -> Result<(Option<i64>, Option<i64>), String> {
    let mut w = None;
    if let Some(s) = within.filter(|s| !s.is_empty()) {
        match parse_seconds(s) {
            Some(v) if (60..=YEAR_S).contains(&v) => w = Some(v),
            _ => return Err("Invalid `within`: use a period between 1 minute and 1 year, e.g. `24h` or `7d`.".into()),
        }
    }
    let mut d = None;
    if let Some(s) = duration.filter(|s| !s.is_empty()) {
        match parse_seconds(s) {
            Some(v) => d = Some(v),
            None => return Err("Invalid `duration`: try `10m`, `1h` or `3d`.".into()),
        }
    }
    if action == "TIMEOUT" && !d.is_some_and(|v| (60..=TIMEOUT_MAX_S).contains(&v)) {
        return Err("Timeout tiers need a duration between 1 minute and 28 days. For longer mutes use the Mute action with a mute role.".into());
    }
    if action == "TEMPBAN" && !d.is_some_and(|v| (60..=YEAR_S).contains(&v)) {
        return Err("Temporary ban tiers need a duration between 1 minute and 1 year.".into());
    }
    if action == "MUTE" && d.is_some_and(|v| !(60..=YEAR_S).contains(&v)) {
        return Err("Mute tiers need a duration between 1 minute and 1 year, or none for \"until unmuted\".".into());
    }
    if (action == "KICK" || action == "BAN") && duration.is_some_and(|s| !s.is_empty()) {
        return Err("Kick and permanent ban tiers don't take a duration (use Temporary ban for a timed ban).".into());
    }
    Ok((w, d))
}

/// The settings mutator of `escalation add`: the new tier list, and whether a tier was replaced.
pub fn add_tier(cur: &GuildSettings, warns: i64, action: &str, within: Option<i64>, dur: Option<i64>) -> Result<(Vec<Tier>, bool), ModConfigError> {
    // The mute role is read from the locked row: no other settings load may happen inside a mutator.
    if action == "MUTE" && cur.mute_role_id.as_deref().is_none_or(str::is_empty) && dur.is_none_or(|d| d > TIMEOUT_MAX_S) {
        return Err(ModConfigError(
            "Mute tiers that are permanent or longer than 28 days need a mute role. Set one with `/modconfig muterole` first.".into(),
        ));
    }
    let mut tiers = normalize_escalation_tiers(cur.infraction_escalation.as_ref());
    let i = tiers.iter().position(|t| t.warn_count == warns && t.within_seconds == within);
    let replaced = i.is_some();
    match i {
        Some(i) => {
            let (id, count_from) = (tiers[i].id, tiers[i].count_from.clone());
            tiers[i] = Tier { id, warn_count: warns, within_seconds: within, action: action.into(), duration_seconds: dur, count_from };
        }
        None => {
            let cap = bot_config::limits::ESCALATION_TIERS;
            if tiers.len() >= cap {
                return Err(ModConfigError(format!("You can have at most {cap} escalation tiers. Remove one first.")));
            }
            let id = tiers.iter().map(|t| t.id).max().unwrap_or(0).max(0) + 1;
            tiers.push(Tier { id, warn_count: warns, within_seconds: within, action: action.into(), duration_seconds: dur, count_from: None });
        }
    }
    Ok((normalize_escalation_tiers(Some(&tiers_value(&tiers))), replaced))
}

/// The text of `escalation add`'s answer.
pub fn add_tier_text(tiers: &[Tier], warns: i64, within: Option<i64>, mod_enabled: bool) -> String {
    let mine = tiers.iter().position(|t| t.warn_count == warns && t.within_seconds == within);
    let lines: Vec<String> = tiers.iter().enumerate().map(|(i, t)| if Some(i) == mine { format!("**{}**", tier_line(t)) } else { tier_line(t) }).collect();
    let mut text = format!("{}\n{}", if mine.is_some() { "All tiers (the one just saved is in bold):" } else { "Saved." }, lines.join("\n"));
    if !mod_enabled {
        text.push_str("\n\nModeration is off: tiers apply after `/module enable moderation`.");
    }
    text
}

// ── Escalation ────────────────────────────────────────────────────────────────

async fn escalation_add(ctx: &CommandCtx, g: &str) -> anyhow::Result<CreateEmbed> {
    let warns = ctx.get_integer("warns").ok_or_else(|| anyhow::anyhow!("Required option \"warns\" not found."))?;
    let action = ctx.get_string("action").ok_or_else(|| anyhow::anyhow!("Required option \"action\" not found."))?;
    let within_str = ctx.get_string("within");
    let duration_str = ctx.get_string("duration");
    let (within, dur) = validate_tier(&action, within_str.as_deref(), duration_str.as_deref()).map_err(refuse)?;
    let replaced = Arc::new(AtomicBool::new(false));
    let (flag, act) = (replaced.clone(), action.clone());
    let saved = ctx
        .app
        .settings()
        .update_settings_with(g, move |cur| {
            let (tiers, was) = add_tier(&cur, warns, &act, within, dur)?;
            flag.store(was, Ordering::SeqCst);
            Ok(SettingsPatch::new().set("infractionEscalation", tiers_value(&tiers)))
        })
        .await?;
    let tiers = normalize_escalation_tiers(saved.infraction_escalation.as_ref());
    let text = add_tier_text(&tiers, warns, within, saved.mod_enabled);
    let title = if replaced.load(Ordering::SeqCst) { "Escalation Tier Updated" } else { "Escalation Tier Added" };
    Ok(success_embed(title, Some(&clamp_text(&text, 4000)), None))
}

async fn escalation_remove(ctx: &CommandCtx, g: &str) -> anyhow::Result<CreateEmbed> {
    let id = ctx.get_integer("tier").ok_or_else(|| anyhow::anyhow!("Required option \"tier\" not found."))?;
    let saved = ctx
        .app
        .settings()
        .update_settings_with(g, move |cur| {
            let mut tiers = normalize_escalation_tiers(cur.infraction_escalation.as_ref());
            let Some(i) = tiers.iter().position(|t| t.id == id) else {
                return Err(refuse(format!("There is no tier T{id}. See `/modconfig escalation list`.")));
            };
            tiers.remove(i);
            Ok(SettingsPatch::new().set("infractionEscalation", tiers_value(&normalize_escalation_tiers(Some(&tiers_value(&tiers))))))
        })
        .await?;
    let tiers = normalize_escalation_tiers(saved.infraction_escalation.as_ref());
    Ok(success_embed("Escalation Tier Removed", Some(&clamp_text(&format!("Removed tier T{id}.\n\n{}", tier_list_text(&tiers)), 4000)), None))
}

/// The `escalation list` embed.
pub fn escalation_list_embed(settings: &GuildSettings) -> CreateEmbed {
    let tiers = normalize_escalation_tiers(settings.infraction_escalation.as_ref());
    let desc = if tiers.is_empty() { Some("No escalation tiers. Add one with `/modconfig escalation add`.") } else { None };
    let mut embed = guild_embed(Some(settings), Some("📈 Escalation Tiers"), desc, Some("Tiers fire once per window. Only active (not pardoned) warnings count."));
    let lines: Vec<String> = tiers.iter().map(tier_line).collect();
    for (i, chunk) in chunk_lines(&lines, 1024).into_iter().enumerate() {
        embed = embed.field(if i == 0 { "Tiers" } else { "\u{200b}" }, chunk, false);
    }
    embed
}

/// The autocomplete of `escalation remove tier:`: memo first, a cold load bounded by the acknowledgement budget.
/// Never fails.
pub async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    if name != "tier" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = match ctx.app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => {
            let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
            let (a, gg) = (ctx.app.clone(), g.clone());
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&gg).await) };
            bot_util::async_util::with_timeout(load, budget, "modconfig autocomplete settings").await.ok().flatten()
        }
    };
    respond_autocomplete(&ctx.responder, &tier_choices(settings.as_deref(), &value)).await;
    Ok(())
}

/// The tier choices whose description contains `typed` (case-insensitive), at most 25.
pub fn tier_choices(settings: Option<&GuildSettings>, typed: &str) -> Vec<Choice> {
    let needle = typed.to_lowercase();
    normalize_escalation_tiers(settings.and_then(|s| s.infraction_escalation.as_ref()))
        .iter()
        .filter(|t| describe_tier(t).to_lowercase().contains(&needle))
        .map(|t| Choice::new(describe_tier(t), json!(t.id)))
        .take(25)
        .collect()
}

// ── Immune roles / moderator roles ────────────────────────────────────────────

/// `ROLE_LISTS[kind]`: (field, cap, label).
fn role_list(kind: &str) -> (&'static str, usize, &'static str) {
    if kind == "immune" {
        ("immuneRoleIds", bot_config::limits::IMMUNE_ROLES, "Immune Roles")
    } else {
        ("modRoleIds", bot_config::limits::MOD_ROLES, "Moderator Roles")
    }
}

fn list_of(s: &GuildSettings, field: &str) -> Vec<String> {
    if field == "immuneRoleIds" { s.immune_role_ids.clone() } else { s.mod_role_ids.clone() }
}

async fn role_list_add(ctx: &CommandCtx, g: &str, kind: &str) -> anyhow::Result<CreateEmbed> {
    let (field, cap, _) = role_list(kind);
    let role = ctx.get_role("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    if role.id.to_string() == g {
        return Err(refuse("@everyone cannot be used here."));
    }
    if kind == "modroles" && role.managed {
        return Err(refuse("That role is managed by an integration."));
    }
    let rid = role.id.to_string();
    let exists = Arc::new(AtomicBool::new(false));
    let (flag, id) = (exists.clone(), rid.clone());
    ctx.app
        .settings()
        .update_settings_with(g, move |cur| {
            let ids = list_of(&cur, field);
            if ids.contains(&id) {
                flag.store(true, Ordering::SeqCst);
                return Ok(SettingsPatch::new());
            }
            if ids.len() >= cap {
                return Err(refuse(format!("You can have at most {cap} roles here.")));
            }
            let mut next = ids;
            next.push(id);
            Ok(SettingsPatch::new().set(field, json!(next)))
        })
        .await?;
    let existed = exists.load(Ordering::SeqCst);
    let mut text = if existed { format!("<@&{rid}> is already on the list.") } else { format!("Added <@&{rid}>.") };
    if kind == "modroles" {
        let bot_name = crate::moderation_service::bot_user(&ctx.app).and_then(|u| u.username).unwrap_or_default();
        text.push_str(&format!(
            "\n\nMembers with <@&{rid}> can now use the bot's moderation commands. Discord shows /ban, /kick… only to members with the matching permission by default; to show them to this role, allow it under Server Settings → Integrations → {bot_name}."
        ));
    } else {
        text.push_str("\n\nMembers with this role cannot be warned, muted, kicked or banned through the bot (except by the server owner) and are skipped by automod.");
    }
    Ok(if existed { info_embed("Already Listed", Some(&text), None) } else { success_embed("Role Added", Some(&clamp_text(&text, 4000)), None) })
}

async fn role_list_remove(ctx: &CommandCtx, g: &str, kind: &str) -> anyhow::Result<CreateEmbed> {
    let (field, _, _) = role_list(kind);
    let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let rid = role.to_string();
    let missing = Arc::new(AtomicBool::new(false));
    let (flag, id) = (missing.clone(), rid.clone());
    ctx.app
        .settings()
        .update_settings_with(g, move |cur| {
            let ids = list_of(&cur, field);
            if !ids.contains(&id) {
                flag.store(true, Ordering::SeqCst);
                return Ok(SettingsPatch::new());
            }
            Ok(SettingsPatch::new().set(field, json!(ids.into_iter().filter(|x| *x != id).collect::<Vec<_>>())))
        })
        .await?;
    Ok(if missing.load(Ordering::SeqCst) {
        info_embed("Not Listed", Some(&format!("<@&{rid}> is not on the list.")), None)
    } else {
        success_embed("Role Removed", Some(&format!("Removed <@&{rid}>.")), None)
    })
}

async fn role_list_show(ctx: &CommandCtx, g: &str, kind: &str, current: Settings) -> anyhow::Result<CreateEmbed> {
    let (field, _, label) = role_list(kind);
    let mut settings = current;
    let mut ids = list_of(&settings, field);
    let mut pruned = 0;
    let gid = GuildId::new(snowflake(g).unwrap_or(1));
    let available = ctx.app.guild_directory().guild_state(g) == bot_core::discord::GuildState::Available;
    if !ids.is_empty() && available {
        // A role missing from the cache is not a deleted role: confirm against the full REST role list.
        if let Ok(roles) = rest(&ctx.app).get_role_ids(gid).await
            && !roles.is_empty()
        {
            let known: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
            let stale: Vec<String> = ids.iter().filter(|id| !known.contains(id)).cloned().collect();
            if !stale.is_empty() {
                let s2 = stale.clone();
                settings = ctx
                    .app
                    .settings()
                    .update_settings_with(g, move |cur| {
                        let kept: Vec<String> = list_of(&cur, field).into_iter().filter(|id| !s2.contains(id)).collect();
                        Ok(SettingsPatch::new().set(field, json!(kept)))
                    })
                    .await?;
                pruned = stale.len();
                ids = list_of(&settings, field);
            }
        }
    }
    let mut lines = Vec::new();
    if pruned > 0 {
        lines.push(format!("Removed {pruned} deleted role(s)."));
    }
    let mentions: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
    lines.push(if ids.is_empty() { "None.".into() } else { join_with_limit(&mentions, ", ", 4000, default_more) });
    if kind == "modroles" {
        lines.push(String::new());
        lines.push("Members with Kick, Ban or Moderate Members also count as moderators.".into());
    }
    Ok(guild_embed(Some(&settings), Some(&format!("🛡️ {label}")), Some(&clamp_text(&lines.join("\n"), 4000)), None))
}

// ── Mute role ─────────────────────────────────────────────────────────────────

/// `DENY`: what the mute role may not do.
pub fn mute_deny() -> Permissions {
    Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::CREATE_PRIVATE_THREADS
        | Permissions::ADD_REACTIONS
        | Permissions::SPEAK
        | Permissions::SEND_VOICE_MESSAGES
        | Permissions::SEND_POLLS
}

/// `OVERWRITE_TYPES` plus categories: text, announcement, voice, stage, forum, media and category channels.
pub fn takes_mute_overwrite(kind: u8) -> bool {
    matches!(kind, 0 | 5 | 2 | 13 | 15 | 16 | 4)
}

/// `applyMuteOverwrites(guild, role)`: denies talking for the mute role in every category and non-thread channel.
/// Never fails; per-channel failures are counted. Returns `(applied, failed)`.
pub async fn apply_mute_overwrites(app: &App, guild_id: GuildId, role: RoleId) -> (usize, usize) {
    let Some(guild) = load_guild(app, guild_id) else { return (0, 0) };
    let targets: Vec<GuildChannel> =
        guild.guild.channels.values().filter(|c| !bot_core::permissions::is_thread(c) && takes_mute_overwrite(u8::from(c.kind))).cloned().collect();
    let mut failed = 0;
    let mut workable = Vec::new();
    for ch in targets {
        if missing_bot_permissions(app, PermTarget::Channel(guild_id, ch.id), &["ViewChannel", "ManageRoles"]).is_empty() {
            workable.push(ch);
        } else {
            failed += 1;
        }
    }
    let r = rest(app);
    let deny = mute_deny();
    let results = bot_util::async_util::map_limit(workable, 3, |ch, _| {
        let r = r.clone();
        async move {
            let existing = ch.permission_overwrites.iter().find(|o| o.kind == PermissionOverwriteType::Role(role));
            let (allow, base_deny) = existing.map(|o| (o.allow, o.deny)).unwrap_or((Permissions::empty(), Permissions::empty()));
            r.deny_for_role(ch.id, role, allow - deny, base_deny | deny, "Mute role setup".into()).await
        }
    })
    .await;
    let applied = results.iter().filter(|r| r.is_ok()).count();
    failed += results.len() - applied;
    (applied, failed)
}

async fn mute_role_command(ctx: &CommandCtx, g: &str, settings: Settings) -> anyhow::Result<CreateEmbed> {
    let gid = GuildId::new(snowflake(g).unwrap_or(1));
    // A role `new_role` just created is in the interaction's resolved roles, not yet in the cache.
    let picked = ctx.get_role("role");
    let role = ctx.get_role_id("role");
    let setup = ctx.get_boolean("setup").unwrap_or(false);
    let clear = ctx.get_boolean("clear").unwrap_or(false);

    if clear {
        ctx.app.settings().update_settings(g, SettingsPatch::new().set("muteRoleId", serde_json::Value::Null)).await?;
        let active: (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM "Infraction" WHERE "guildId" = $1 AND "type" = 'MUTE' AND "active" = true"#)
            .bind(g)
            .fetch_one(ctx.app.db())
            .await
            .map_err(|e| anyhow::Error::new(bot_db::DbError::from(e)))?;
        let mut text = "The mute role was cleared. Mutes of 28 days or less use Discord timeouts.".to_string();
        if active.0 > 0 {
            text.push_str(&format!(
                "\n\n{} role mute(s) are still active. Scheduled mutes still expire on time, but `/unmute` can no longer remove the old role; remove it by hand if needed.",
                active.0
            ));
        }
        return Ok(success_embed("Mute Role Cleared", Some(&text), None));
    }

    let guild = load_guild(&ctx.app, gid);
    let executor = match (&guild, ctx.member()) {
        (Some(lg), Some(m)) => Some(lg.member_view(m)),
        _ => None,
    };
    let check = |role_id: RoleId| -> anyhow::Result<()> {
        let chk = match &guild {
            Some(lg) => {
                let fresh = picked.as_ref().filter(|r| r.id == role_id).map(|r| crate::guild_ops::RoleView {
                    id: r.id.to_string(),
                    name: r.name.clone(),
                    position: usize::from(r.position),
                    permissions: r.permissions,
                    managed: r.managed,
                });
                let view = lg.view.role(&role_id.to_string()).cloned().or(fresh);
                lg.view.check_assignable_role(view.as_ref(), executor.as_ref(), false)
            }
            None => bot_core::interaction_helpers::RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
        };
        if chk.ok { Ok(()) } else { Err(refuse(chk.reason.unwrap_or_default())) }
    };

    let target: RoleId = if let Some(r) = role {
        check(r)?;
        r
    } else if setup {
        if !missing_bot_permissions(&ctx.app, PermTarget::Guild(gid), &["ManageRoles"]).is_empty() {
            return Ok(error_embed("Missing Permissions", Some("I need **Manage Roles** to create the mute role."), None));
        }
        // Reuse the configured role instead of creating a duplicate.
        let configured =
            settings.mute_role_id.as_deref().filter(|id| guild.as_ref().is_some_and(|lg| lg.view.role(id).is_some())).and_then(snowflake).map(RoleId::new);
        match configured {
            Some(r) => {
                check(r)?;
                r
            }
            None => {
                let reason = clamp_text(&format!("Mute role created by {} via /modconfig", ctx.user().name), 512);
                let created = rest(&ctx.app).create_role(gid, "Muted".into(), reason).await.map_err(anyhow::Error::new)?;
                // `guild.roles.create` adds the role to the cache: the next mute finds it at once.
                bot_core::discord::cache_created_role(&ctx.app, gid, &created);
                created.id
            }
        }
    } else {
        let text = match settings.mute_role_id.as_deref().filter(|s| !s.is_empty()) {
            Some(current) if guild.as_ref().is_some_and(|lg| lg.view.role(current).is_some()) => format!("The mute role is <@&{current}>."),
            Some(_) => "The mute role is set, but the role no longer exists. Set a new one with `/modconfig muterole`.".to_string(),
            None => "No mute role is set. Mutes of 28 days or less use Discord timeouts.".to_string(),
        };
        return Ok(guild_embed(Some(&settings), Some("🔇 Mute Role"), Some(&text), None));
    };

    ctx.app.settings().update_settings(g, SettingsPatch::new().set("muteRoleId", target.to_string())).await?;
    let mut lines = vec![format!("**Mute role:** <@&{target}>")];
    if setup {
        let (applied, failed) = apply_mute_overwrites(&ctx.app, gid, target).await;
        let fail_note = if failed > 0 { format!(", {failed} failed (missing access)") } else { String::new() };
        lines.push(format!("**Channel overrides:** {applied} applied{fail_note}"));
        lines.push("New channels need the override too: run `/modconfig muterole setup:true` again after adding channels.".into());
    }
    lines.push("Mutes of 28 days or less still use Discord timeouts; longer or permanent mutes use this role.".into());
    Ok(success_embed("Mute Role Saved", Some(&clamp_text(&lines.join("\n"), 4000)), None))
}

// ── Case log, visibility, DM ──────────────────────────────────────────────────

async fn log_channel_command(ctx: &CommandCtx, g: &str) -> anyhow::Result<CreateEmbed> {
    let Some(channel) = ctx.get_channel_id("channel") else {
        ctx.app.settings().update_settings(g, SettingsPatch::new().set("modLogChannelId", serde_json::Value::Null)).await?;
        return Ok(success_embed("Moderation Log", Some("Case logging turned off."), None));
    };
    let gid = GuildId::new(snowflake(g).unwrap_or(1));
    // The picked channel (discord.js resolves it from the interaction even when it is not cached).
    let u = bot_core::interaction_helpers::check_channel_usable_fetch(&ctx.app, gid, Some(channel), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).await;
    if !u.ok {
        return Err(refuse(if u.missing.is_empty() {
            format!("I can't post in <#{channel}>: {}", u.reason.unwrap_or_default())
        } else {
            format!("I can't post in <#{channel}>: missing **{}**.", bot_core::permissions::permission_names(&u.missing).join(", "))
        }));
    }
    ctx.app.settings().update_settings(g, SettingsPatch::new().set("modLogChannelId", channel.to_string())).await?;
    let note = serenity::all::CreateMessage::new().embed(success_embed("Moderation Log", Some("Moderation cases will be posted here."), None));
    let _ = rest(&ctx.app).send_message(ChannelId::new(channel.get()), note).await;
    Ok(success_embed(
        "Moderation Log",
        Some(&format!("Case logs will go to <#{channel}>. They are posted even when the Audit Logs module is off.")),
        None,
    ))
}

/// `VISIBILITY_TEXT[mode]`.
pub fn visibility_text(mode: &str) -> &'static str {
    match mode {
        "ANONYMOUS" => {
            "A public notice is posted without the moderator; the moderator gets a private confirmation. /case replies (history included) become private."
        }
        "PRIVATE" => "Only the moderator sees confirmations. /case replies (history included) are private.",
        _ => "Action confirmations are posted publicly.",
    }
}

async fn visibility_command(ctx: &CommandCtx, g: &str) -> anyhow::Result<CreateEmbed> {
    let mode = ctx.get_string("mode").ok_or_else(|| anyhow::anyhow!("Required option \"mode\" not found."))?;
    ctx.app.settings().update_settings(g, SettingsPatch::new().set("modVisibility", mode.clone())).await?;
    let mut chars = mode.chars();
    let pretty = match chars.next() {
        Some(c) => format!("{c}{}", chars.as_str().to_lowercase()),
        None => String::new(),
    };
    Ok(success_embed("Command Visibility", Some(&format!("**{pretty}**\n{}", visibility_text(&mode))), None))
}

async fn dm_command(ctx: &CommandCtx, g: &str) -> anyhow::Result<CreateEmbed> {
    let enabled = ctx.get_boolean("enabled").ok_or_else(|| anyhow::anyhow!("Required option \"enabled\" not found."))?;
    ctx.app.settings().update_settings(g, SettingsPatch::new().set("modDmTargets", enabled)).await?;
    let text = if enabled {
        "Members will be DMed when they are warned, timed out, muted, kicked or banned."
    } else {
        "Members will no longer be DMed about moderation actions."
    };
    Ok(success_embed("Member DMs", Some(text), None))
}

// ── Show ──────────────────────────────────────────────────────────────────────

/// The `Escalation` field of `/modconfig show`: the first 1024-character chunk, plus "… N more".
pub fn escalation_field(tiers: &[Tier]) -> String {
    if tiers.is_empty() {
        return "None".into();
    }
    let lines: Vec<String> = tiers.iter().map(tier_line).collect();
    let chunks = chunk_lines(&lines, 1024);
    let first = chunks.first().cloned().unwrap_or_default();
    let shown = first.split('\n').count();
    if shown < tiers.len() { clamp_text(&format!("{first}\n… {} more", tiers.len() - shown), 1024) } else { first }
}

async fn show_command(ctx: &CommandCtx, g: &str, settings: Settings) -> anyhow::Result<CreateEmbed> {
    let gid = GuildId::new(snowflake(g).unwrap_or(1));
    let tiers = normalize_escalation_tiers(settings.infraction_escalation.as_ref());
    let mut case_log = "Not set".to_string();
    if let Some(id) = settings.mod_log_channel_id.as_deref().filter(|s| !s.is_empty()) {
        // `guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null)`.
        let usable = match snowflake(id) {
            Some(c) => {
                bot_core::interaction_helpers::check_channel_usable_fetch(&ctx.app, gid, Some(ChannelId::new(c)), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS)
                    .await
                    .ok
            }
            None => false,
        };
        case_log = if usable { format!("<#{id}>") } else { format!("<#{id}> ⚠️ unusable") };
    }
    let mut mute_role = "Not set".to_string();
    if let Some(id) = settings.mute_role_id.as_deref().filter(|s| !s.is_empty()) {
        let exists = load_guild(&ctx.app, gid).is_some_and(|lg| lg.view.role(id).is_some());
        mute_role = if exists { format!("<@&{id}>") } else { "⚠️ deleted".into() };
    }
    let overdue = count_overdue_bans(&ctx.app, g).await;
    let desc = (overdue > 0)
        .then(|| format!("⚠️ {overdue} temporary ban(s) expired more than 30 days ago and were not lifted automatically. Use /unban to lift them."));
    let visibility = match settings.mod_visibility {
        ModVisibility::Public => "PUBLIC",
        ModVisibility::Anonymous => "ANONYMOUS",
        ModVisibility::Private => "PRIVATE",
    };
    let embed = guild_embed(Some(&settings), Some("🔨 Moderation Settings"), desc.as_deref(), None).fields(vec![
        ("Module", if settings.mod_enabled { "✅ On".to_string() } else { "❌ Off (enable with /module enable moderation)".to_string() }, true),
        ("Case log", clamp_text(&case_log, 1024), true),
        ("Visibility", visibility.to_string(), true),
        ("DM members", if settings.mod_dm_targets { "On".to_string() } else { "Off".to_string() }, true),
        ("Mute role", clamp_text(&mute_role, 1024), true),
        ("Moderator roles", roles_line(&settings.mod_role_ids), false),
        ("Immune roles", roles_line(&settings.immune_role_ids), false),
        ("Escalation", escalation_field(&tiers), false),
    ]);
    Ok(embed)
}

// ── Entry point ───────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    // Public, like every admin config command.
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let result = run(&mut ctx).await;
    let embed = match result {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(()),
        Err(err) => {
            if let Some(ie) = err.downcast_ref::<bot_core::interaction::InteractionError>()
                && bot_core::interaction_helpers::is_interaction_gone(ie)
            {
                return Ok(());
            }
            match err.downcast_ref::<ModConfigError>() {
                Some(m) => error_embed("Not Saved", Some(&m.0), None),
                None => {
                    bot_util::log_error!("/modconfig failed".to_string(), bot_core::error::log_arg(&err));
                    error_embed("Not Saved", Some("Saving failed. Please try again."), None)
                }
            }
        }
    };
    let _ = ctx.edit_reply(Payload::embed(embed).to_edit()).await;
    Ok(())
}

async fn run(ctx: &mut CommandCtx) -> anyhow::Result<Option<CreateEmbed>> {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let targets = match (group.as_deref(), sub.as_str()) {
        (Some("modroles"), "add") => NewOptions { required: &["role"], private: &[] },
        (None, "logchannel") => NewOptions { required: &[], private: &["channel"] },
        _ => NewOptions { required: &[], private: &[] },
    };
    if !resolve_new_options(ctx, targets).await {
        return Ok(None);
    }
    let g = ctx.guild_id().map(|g| g.to_string()).ok_or_else(|| anyhow::anyhow!("/modconfig outside a guild"))?;
    let needs_settings = matches!(sub.as_str(), "list" | "show" | "muterole");
    let settings = if needs_settings { ctx.app.settings().get_settings_safe(&g).await } else { None };
    if needs_settings && settings.is_none() {
        return Ok(Some(error_embed("Settings Unavailable", Some("Server settings are temporarily unavailable. Please try again in a moment."), None)));
    }
    let ctx = &*ctx;
    let embed = match (group.as_deref(), sub.as_str()) {
        (Some("escalation"), "add") => escalation_add(ctx, &g).await?,
        (Some("escalation"), "remove") => escalation_remove(ctx, &g).await?,
        (Some("escalation"), _) => escalation_list_embed(settings.as_deref().ok_or_else(|| anyhow::anyhow!("no settings"))?),
        (Some(kind @ ("immune" | "modroles")), "add") => role_list_add(ctx, &g, kind).await?,
        (Some(kind @ ("immune" | "modroles")), "remove") => role_list_remove(ctx, &g, kind).await?,
        (Some(kind @ ("immune" | "modroles")), _) => role_list_show(ctx, &g, kind, settings.ok_or_else(|| anyhow::anyhow!("no settings"))?).await?,
        (_, "muterole") => mute_role_command(ctx, &g, settings.ok_or_else(|| anyhow::anyhow!("no settings"))?).await?,
        (_, "logchannel") => log_channel_command(ctx, &g).await?,
        (_, "visibility") => visibility_command(ctx, &g).await?,
        (_, "dm") => dm_command(ctx, &g).await?,
        _ => show_command(ctx, &g, settings.ok_or_else(|| anyhow::anyhow!("no settings"))?).await?,
    };
    Ok(Some(embed))
}
