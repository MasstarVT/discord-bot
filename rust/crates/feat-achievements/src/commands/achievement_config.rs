//! Port of `src/commands/admin/achievement-config.js`: `/achievement-config` create, edit, delete, enable, disable,
//! list, info, reset (MEE6 /achievements-reset), preset, tier add|edit|remove and announce mode|channel|message. An
//! admin config command: replies are public except the delete/reset prompts, and it keeps working while the
//! Achievements module is off (`gated = false`). Business logic and database access live in
//! [`crate::achievement_service`].

use std::collections::HashSet;

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, check_assignable_role_cached, check_channel_usable, respond_autocomplete, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_core::settings::Settings;
use bot_db::Achievement;
use bot_util::text::{clamp_text, format_number, join_with_limit};
use bot_util::time::discord_timestamp;
use chrono::{DateTime, Utc};
use serenity::all::{ChannelId, GuildId, Permissions, RoleId};
use serenity::builder::CreateEmbed;

use crate::achievement_service::{
    self as svc, AchievementPatch, DEFAULT_ACHIEVEMENT_MESSAGE, GOAL_INPUT_MAX, LIST_PAGE_SIZE, MAX_REWARD_AMOUNT, MODULE_OFF_NOTE, NewAchievement, Reward,
    TIER_EMOJIS, TIER_NAMES, Tier, TierChange, WriteRefusal, achievement_status, badge_of, format_rewards, goal_max, goal_text, merge_rewards,
    normalize_achievement_name, normalize_badge_emoji, normalize_rewards, normalize_tiers, parse_deadline, parse_goals, reward_unavailable_message,
    serialize_tiers, tier_inputs_json, tiers_json, trigger_emoji, trigger_label, validate_tiers, with_config_lock,
};

const TEXT_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn ach() -> Opt {
    Opt::string("achievement", "Achievement").required(true).autocomplete(true).max_length(100)
}

fn reward_opts(s: Opt) -> Opt {
    s.option(Opt::role("role_add", "Reward: role to give"))
        .option(new_role_option("role_add"))
        .option(Opt::role("role_remove", "Reward: role to take away"))
        .option(Opt::integer("xp", "Reward: XP (needs Levels)").min_value(1).max_value(MAX_REWARD_AMOUNT as f64))
        .option(Opt::integer("coins", "Reward: coins (needs Economy)").min_value(1).max_value(MAX_REWARD_AMOUNT as f64))
}

/// The command's data.
pub fn data() -> SlashCommand {
    let triggers = Opt::string("trigger", "What members do to progress")
        .required(true)
        .choice("Messages sent", "MESSAGES")
        .choice("Reactions added", "REACTIONS")
        .choice("Level reached", "LEVEL")
        .choice("Server boosts", "BOOST")
        .choice("Shop items bought", "PURCHASES");
    let tiers = bot_config::limits::ACHIEVEMENT_TIERS as f64;
    SlashCommand::new("achievement-config", "Set up achievements (MEE6 Achievements, /achievements-reset)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            reward_opts(
                Opt::sub_command("create", "Create an achievement; rewards apply at every tier")
                    .option(Opt::string("name", "Achievement name").required(true).max_length(50))
                    .option(triggers)
                    .option(Opt::string("goals", "Goal per tier, low to high: 100 or 100,500,1k (max 4)").required(true).max_length(60)),
            )
            .option(Opt::string("description", "Short description").max_length(200))
            .option(Opt::string("emoji", "Badge emoji").max_length(64))
            .option(Opt::boolean("track_past", "Count past activity (default True)"))
            .option(Opt::string("deadline", "Last moment to progress, e.g. 2026-12-31 23:59").max_length(40)),
        )
        .option(
            Opt::sub_command("edit", "Change name, description, badge or deadline")
                .option(ach())
                .option(Opt::string("name", "New name").max_length(50))
                .option(Opt::string("description", "New description (\"none\" to clear)").max_length(200))
                .option(Opt::string("emoji", "New badge emoji (\"none\" to clear)").max_length(64))
                .option(Opt::boolean("track_past", "Count past activity"))
                .option(Opt::string("deadline", "New deadline (\"none\" to clear)").max_length(40)),
        )
        .option(Opt::sub_command("delete", "Delete an achievement and its progress").option(ach()))
        .option(Opt::sub_command("enable", "Turn an achievement on").option(ach()))
        .option(Opt::sub_command("disable", "Turn an achievement off (progress is kept)").option(ach()))
        .option(Opt::sub_command("list", "List achievements"))
        .option(Opt::sub_command("info", "Show tiers, rewards and unlock counts").option(ach()))
        .option(
            Opt::sub_command("reset", "Reset a member's progress (MEE6 /achievements-reset)")
                .option(Opt::user("member", "Member to reset").required(true))
                .option(Opt::string("achievement", "Only this achievement (default: all)").autocomplete(true).max_length(100)),
        )
        .option(
            Opt::sub_command("preset", "Add a pre-made achievement")
                .option(
                    Opt::string("template", "Pre-made achievement")
                        .required(true)
                        .choice("Chatterbox (messages)", "chatterbox")
                        .choice("Booster (boosts)", "booster")
                        .choice("Level 10 (level)", "level10"),
                )
                .option(Opt::string("name", "Custom name").max_length(50)),
        )
        .option(
            Opt::sub_command_group("tier", "Manage tiers (Bronze, Silver, Gold, Diamond)")
                .option(reward_opts(
                    Opt::sub_command("add", "Add a tier above the current top tier")
                        .option(ach())
                        .option(Opt::integer("goal", "Goal for the new tier").required(true).min_value(1).max_value(GOAL_INPUT_MAX as f64)),
                ))
                .option(
                    reward_opts(
                        Opt::sub_command("edit", "Change a tier's goal or rewards")
                            .option(ach())
                            .option(Opt::integer("tier", "Tier number (1 = Bronze)").required(true).min_value(1).max_value(tiers))
                            .option(Opt::integer("goal", "New goal").min_value(1).max_value(GOAL_INPUT_MAX as f64)),
                    )
                    .option(Opt::boolean("clear_rewards", "Remove existing rewards first")),
                )
                .option(
                    Opt::sub_command("remove", "Remove a tier")
                        .option(ach())
                        .option(Opt::integer("tier", "Tier number (1 = Bronze)").required(true).min_value(1).max_value(tiers)),
                ),
        )
        .option(
            Opt::sub_command_group("announce", "Unlock announcements")
                .option(
                    Opt::sub_command("mode", "Off, current channel, DM or a set channel")
                        .option(
                            Opt::string("mode", "Where to announce")
                                .required(true)
                                .choice("Disabled", "OFF")
                                .choice("Current channel", "CURRENT")
                                .choice("Private message", "DM")
                                .choice("Custom channel", "CHANNEL"),
                        )
                        .option(Opt::channel("channel", "Channel for Custom channel mode").channel_types(&TEXT_TYPES))
                        .option(new_channel_option("channel", "channel"))
                        .option(Opt::boolean("embed", "Send unlocks as an embed")),
                )
                .option(
                    Opt::sub_command("channel", "Set the announcement channel")
                        .option(Opt::channel("channel", "Channel to announce in").channel_types(&TEXT_TYPES))
                        .option(new_channel_option("channel", "channel")),
                )
                .option(
                    Opt::sub_command("message", "Set the announcement text (blank = default)")
                        .option(Opt::string("text", "Text with {player} {achievement} {tier} {rewards}").max_length(1500)),
                ),
        )
}

/// `/achievement-config` (`module 'achievements'`, `gated = false`, ADMIN for every subcommand).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Achievements), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

/// `MODE_LABELS`.
pub fn mode_label(mode: &str) -> &str {
    match mode {
        "OFF" => "Disabled",
        "CURRENT" => "Current channel",
        "DM" => "Private message",
        "CHANNEL" => "Custom channel",
        other => other,
    }
}

/// A refusal: an error embed's title and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub title: String,
    pub error: String,
}

fn fail(title: &str, error: impl Into<String>) -> Fail {
    Fail { title: title.into(), error: error.into() }
}

fn not_found() -> Fail {
    fail("Achievement Not Found", "I couldn't find that achievement. Pick one from the list.")
}

const GENERIC_ERROR: (&str, &str) = ("Something Went Wrong", "I could not save that. Please try again in a moment.");

type Out = Result<CreateEmbed, Fail>;

/// The context every handler gets (`{ interaction, settings, guildId, moduleOff }`).
struct C {
    app: App,
    guild_id: GuildId,
    g: String,
    settings: Settings,
    module_off: bool,
    /// The guild's role cache (`None` when the guild is not cached).
    roles: Option<HashSet<String>>,
}

fn ok_embed(module_off: bool, title: &str, description: &str) -> Out {
    Ok(success_embed(title, Some(&format!("{description}{}", if module_off { MODULE_OFF_NOTE } else { "" })), None))
}

fn settings_tz(s: &Settings) -> String {
    s.timezone.clone()
}

/// `dependencyWarnings(trigger, settings)`: warning lines (each starts with a newline).
pub fn dependency_warnings(trigger: &str, settings: &bot_db::GuildSettings) -> String {
    let mut out = String::new();
    if trigger == "LEVEL" && !is_module_enabled(Some(settings), Some(ModuleKey::Leveling)) {
        out.push_str("\n⚠️ Level achievements only progress while the ⭐ Levels module is on.");
    }
    if trigger == "PURCHASES" && !is_module_enabled(Some(settings), Some(ModuleKey::Economy)) {
        out.push_str("\n⚠️ Purchase achievements only progress while the 🪙 Economy module is on.");
    }
    let mode = settings.achievement_announce_mode.as_str();
    if trigger == "BOOST" && (mode == "CURRENT" || mode == "CHANNEL") && crate::birthday_service::set_id(&settings.achievement_channel_id).is_none() {
        out.push_str("\nℹ️ Boosts happen outside any channel. Set `/achievement-config announce channel` to announce them.");
    }
    out
}

fn deadline_text(d: Option<DateTime<Utc>>) -> String {
    d.map(|d| discord_timestamp(&d, "f")).unwrap_or_else(|| "none".into())
}

fn trigger_of(a: &Achievement) -> &'static str {
    a.trigger.as_str()
}

/// `achievementSummary(a, settings, guild)`: the multi-line description of an achievement row.
pub fn achievement_summary(a: &Achievement, settings: &bot_db::GuildSettings, roles: Option<&HashSet<String>>, now_ms: i64) -> String {
    let tiers = normalize_tiers(&a.tiers);
    let trigger = trigger_of(a);
    let status = achievement_status(a.enabled, a.deadline.map(|d| d.timestamp_millis()), now_ms);
    let mut lines = vec![format!("{} **{}** · {} · {status}", badge_of(a.emoji.as_deref(), trigger), a.name, trigger_label(trigger).unwrap_or_default())];
    if let Some(d) = a.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(format!("*{d}*"));
    }
    for (i, t) in tiers.iter().enumerate() {
        let rewards = if t.rewards.is_empty() { String::new() } else { format!(" → {}", format_rewards(&t.rewards, roles, Some(settings))) };
        lines.push(format!("{} {} — {}{rewards}", TIER_EMOJIS[i], TIER_NAMES[i], goal_text(trigger, t.goal)));
    }
    lines.push(format!("Past activity: {} · Deadline: {}", if a.track_past_progress { "counted" } else { "not counted" }, deadline_text(a.deadline)));
    lines.join("\n")
}

fn reward_count_of(a: &Achievement) -> usize {
    normalize_tiers(&a.tiers).iter().map(|t| t.rewards.len()).sum()
}

/// Runs `f` under the guild's config lock and refreshes the achievement index afterwards when it succeeded.
async fn locked<F, Fut>(c: &C, refresh: bool, f: F) -> anyhow::Result<Out>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<Out>> + Send + 'static,
{
    let (app, g) = (c.app.clone(), c.g.clone());
    with_config_lock(&c.app, &c.g, move || async move {
        let out = f().await?;
        if refresh && out.is_ok() {
            svc::refresh_achievement_cache(&app, &g).await;
        }
        Ok(out)
    })
    .await
}

/// `send(c, out, { followUp })`: an error embed or the success embed; nothing pings.
async fn send(ctx: &CommandCtx, out: Out, follow_up: bool) -> anyhow::Result<()> {
    let ok = out.is_ok();
    let embed = match out {
        Ok(e) => e,
        Err(f) => error_embed(&f.title, Some(&f.error), None),
    };
    let payload = Payload::embed(embed).no_mentions();
    if follow_up {
        ctx.follow_up(payload.ephemeral(!ok).to_follow_up()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    } else {
        ctx.edit_reply(payload.to_edit()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

fn role_id_of(ctx: &CommandCtx, name: &str) -> Option<RoleId> {
    ctx.get_role_id(name)
}

/// `collectRewards(interaction, settings)`: reads the four reward options and checks them against the guild.
fn collect_rewards(ctx: &CommandCtx, c: &C) -> Result<(Vec<Reward>, Vec<&'static str>), Fail> {
    let role_add = role_id_of(ctx, "role_add");
    let role_remove = role_id_of(ctx, "role_remove");
    let xp = ctx.get_integer("xp");
    let coins = ctx.get_integer("coins");
    if role_add.is_some() && role_add == role_remove {
        return Err(fail("Invalid Rewards", "A tier cannot give and take the same role."));
    }
    if let Some(r) = role_add {
        let chk = check_assignable_role_cached(&c.app, c.guild_id, r, ctx.member(), false);
        if !chk.ok {
            return Err(fail("Role Not Allowed", format!("<@&{r}>: {}", chk.reason.unwrap_or_default())));
        }
    }
    if let Some(r) = role_remove {
        let chk = check_assignable_role_cached(&c.app, c.guild_id, r, ctx.member(), true);
        if !chk.ok {
            return Err(fail("Role Not Allowed", format!("<@&{r}>: {}", chk.reason.unwrap_or_default())));
        }
    }
    if xp.is_some()
        && let Some(msg) = reward_unavailable_message(&c.app, RewardKind::Xp, Some(&c.settings))
    {
        return Err(fail("Reward Unavailable", msg));
    }
    if coins.is_some()
        && let Some(msg) = reward_unavailable_message(&c.app, RewardKind::Coins, Some(&c.settings))
    {
        return Err(fail("Reward Unavailable", msg));
    }
    let mut given = Vec::new();
    let mut list = Vec::new();
    if let Some(r) = role_add {
        given.push("ROLE_ADD");
        list.push(Reward::RoleAdd(r.to_string()).to_json());
    }
    if let Some(r) = role_remove {
        given.push("ROLE_REMOVE");
        list.push(Reward::RoleRemove(r.to_string()).to_json());
    }
    if let Some(n) = xp {
        given.push("XP");
        list.push(Reward::Xp(n).to_json());
    }
    if let Some(n) = coins {
        given.push("COINS");
        list.push(Reward::Coins(n).to_json());
    }
    Ok((normalize_rewards(&serde_json::Value::Array(list)), given))
}

/// The options shared by create and edit (`parseCommon`). Outer `None` = not given; inner `None` = cleared.
#[derive(Debug, Clone, Default)]
struct Common {
    name: Option<String>,
    emoji: Option<Option<String>>,
    description: Option<Option<String>>,
    deadline: Option<Option<DateTime<Utc>>>,
    track: Option<bool>,
}

fn has_custom_emoji(app: &App, id: &str) -> bool {
    let Some(d) = app.discord() else { return false };
    let Ok(eid) = id.parse::<u64>() else { return false };
    d.cache.guilds().into_iter().any(|g| d.cache.guild(g).is_some_and(|g| g.emojis.contains_key(&serenity::all::EmojiId::new(eid))))
}

fn parse_common(ctx: &CommandCtx, c: &C, editing: bool) -> Result<Common, Fail> {
    let mut out = Common::default();
    if let Some(raw) = ctx.get_string("name") {
        let Some(name) = normalize_achievement_name(Some(&raw)) else { return Err(fail("Invalid Name", "Names must be 1-50 characters.")) };
        out.name = Some(name);
    }
    if let Some(raw) = ctx.get_string("emoji") {
        let e = normalize_badge_emoji(&raw, |id| has_custom_emoji(&c.app, id), editing).map_err(|e| fail("Invalid Emoji", e))?;
        out.emoji = Some(e);
    }
    if let Some(raw) = ctx.get_string("description") {
        let t = bot_util::js::trim(&raw);
        out.description = Some(if t.is_empty() || (editing && bot_util::js::lower(t) == "none") { None } else { Some(t.to_string()) });
    }
    if let Some(raw) = ctx.get_string("deadline") {
        let d = parse_deadline(&raw, &settings_tz(&c.settings), &c.app.clock().now(), editing).map_err(|e| fail("Invalid Deadline", e))?;
        out.deadline = Some(d);
    }
    out.track = ctx.get_boolean("track_past");
    Ok(out)
}

fn creation_refusal(r: WriteRefusal, name: &str, preset: bool) -> Fail {
    match r {
        WriteRefusal::Limit => fail("Limit Reached", format!("A server can have at most {} achievements. Delete one first.", bot_config::limits::ACHIEVEMENTS)),
        _ if preset => fail("Name Taken", format!("An achievement named **{name}** already exists. Pass `name:` to add it under another name.")),
        _ => fail("Name Taken", format!("An achievement named **{name}** already exists.")),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn create(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    // Checks run in the spec's order (name, goals, emoji, description, deadline, rewards); the first failure replies.
    if normalize_achievement_name(ctx.get_string("name").as_deref()).is_none() {
        return send(ctx, Err(fail("Invalid Name", "Names must be 1-50 characters.")), false).await;
    }
    let trigger = ctx.get_string("trigger").unwrap_or_default();
    let goals = match parse_goals(&ctx.get_string("goals").unwrap_or_default(), &trigger) {
        Ok(g) => g,
        Err(e) => return send(ctx, Err(fail("Invalid Goals", e)), false).await,
    };
    let common = match parse_common(ctx, c, false) {
        Ok(v) => v,
        Err(f) => return send(ctx, Err(f), false).await,
    };
    let (rewards, _) = match collect_rewards(ctx, c) {
        Ok(r) => r,
        Err(f) => return send(ctx, Err(f), false).await,
    };
    let name = common.name.clone().unwrap_or_default();
    let data = NewAchievement {
        name: name.clone(),
        description: common.description.clone().flatten(),
        emoji: common.emoji.clone().flatten(),
        trigger: trigger.clone(),
        track_past_progress: common.track.unwrap_or(true),
        deadline: common.deadline.flatten(),
        tiers: tier_inputs_json(&goals.iter().map(|g| (*g, rewards.clone())).collect::<Vec<_>>()),
    };
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let multi = goals.len() > 1;
    let out = locked(c, true, move || async move {
        let row = match svc::create_achievement(&app, &g, &data).await? {
            Ok(r) => r,
            Err(r) => return Ok(Err(creation_refusal(r, &name, false))),
        };
        let extra = if !rewards.is_empty() && multi { "\nℹ️ These rewards are given at every tier. Use `/achievement-config tier edit` for per-tier rewards." } else { "" };
        let text = format!("{}{}{extra}", achievement_summary(&row, &settings, roles.as_ref(), app.clock().now_ms()), dependency_warnings(&trigger, &settings));
        Ok(ok_embed(module_off, "Achievement Created", &text))
    })
    .await?;
    send(ctx, out, false).await
}

async fn edit(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let input = ctx.get_string("achievement").unwrap_or_default();
    let any = ["name", "description", "emoji", "track_past", "deadline"].iter().any(|n| ctx.has_option(n));
    let common = parse_common(ctx, c, true);
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let Some(a) = svc::resolve_achievement(app.db(), &g, &input).await? else { return Ok(Err(not_found())) };
        if !any {
            return Ok(Err(fail("Nothing To Change", "Pass at least one option to change.")));
        }
        let v = match common {
            Ok(v) => v,
            Err(f) => return Ok(Err(f)),
        };
        if let Some(name) = &v.name
            && svc::name_taken(app.db(), &g, name, Some(&a.id)).await?
        {
            return Ok(Err(fail("Name Taken", format!("An achievement named **{name}** already exists."))));
        }
        let patch = AchievementPatch {
            name: v.name.clone(),
            description: v.description.clone(),
            emoji: v.emoji.clone(),
            track_past_progress: v.track,
            deadline: v.deadline,
            enabled: None,
        };
        match svc::update_achievement(app.db(), &g, &a.id, &patch).await? {
            Ok(()) => {}
            Err(WriteRefusal::NameTaken) => {
                return Ok(Err(fail("Name Taken", format!("An achievement named **{}** already exists.", v.name.clone().unwrap_or_default()))));
            }
            Err(_) => return Ok(Err(not_found())),
        }
        let mut changes = Vec::new();
        if let Some(n) = &v.name {
            changes.push(format!("• **Name:** {} → {n}", a.name));
        }
        if let Some(d) = &v.description {
            changes.push(format!("• **Description:** {} → {}", a.description.as_deref().unwrap_or("none"), d.as_deref().unwrap_or("none")));
        }
        if let Some(e) = &v.emoji {
            changes.push(format!("• **Badge:** {} → {}", a.emoji.as_deref().unwrap_or("default"), e.as_deref().unwrap_or("default")));
        }
        let counted = |b: bool| if b { "counted" } else { "not counted" };
        if let Some(t) = v.track {
            changes.push(format!("• **Past activity:** {} → {}", counted(a.track_past_progress), counted(t)));
        }
        if let Some(d) = v.deadline {
            changes.push(format!("• **Deadline:** {} → {}", deadline_text(a.deadline), deadline_text(d)));
        }
        let mut merged = a.clone();
        if let Some(n) = v.name {
            merged.name = n;
        }
        if let Some(d) = v.description {
            merged.description = d;
        }
        if let Some(e) = v.emoji {
            merged.emoji = e;
        }
        if let Some(t) = v.track {
            merged.track_past_progress = t;
        }
        if let Some(d) = v.deadline {
            merged.deadline = d;
        }
        let mut text = format!("{}\n\n{}", changes.join("\n"), achievement_summary(&merged, &settings, roles.as_ref(), app.clock().now_ms()));
        if v.track.is_some() {
            text.push_str("\nℹ️ This applies to members who have no progress here yet; existing progress is kept.");
        }
        Ok(ok_embed(module_off, "Achievement Updated", &text))
    })
    .await?;
    send(ctx, out, false).await
}

async fn remove(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let Some(a) = svc::resolve_achievement(c.app.db(), &c.g, &ctx.get_string("achievement").unwrap_or_default()).await? else {
        return send(ctx, Err(not_found()), false).await;
    };
    let yes = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete achievement?".into()),
            description: Some(format!("Delete **{}** and every member's progress on it? Roles, XP and coins already given are kept.", a.name)),
            confirm_label: "Delete".into(),
            ..Default::default()
        },
    )
    .await;
    if !yes {
        return Ok(());
    }
    let (app, g, module_off) = (c.app.clone(), c.g.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let count = svc::delete_achievement(app.db(), &g, &a.id).await?;
        Ok(if count == 0 { Err(fail("Not Found", "That achievement was already deleted.")) } else { ok_embed(module_off, "Achievement Deleted", &format!("**{}** was deleted.", a.name)) })
    })
    .await?;
    send(ctx, out, true).await
}

async fn toggle(ctx: &CommandCtx, c: &C, enabled: bool) -> anyhow::Result<()> {
    let input = ctx.get_string("achievement").unwrap_or_default();
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let Some(a) = svc::resolve_achievement(app.db(), &g, &input).await? else { return Ok(Err(not_found())) };
        if a.enabled == enabled {
            return Ok(Ok(info_embed("No Change", Some(&format!("**{}** is already {}.", a.name, if enabled { "on" } else { "off" })), None)));
        }
        if svc::update_achievement(app.db(), &g, &a.id, &AchievementPatch { enabled: Some(enabled), ..Default::default() }).await?.is_err() {
            return Ok(Err(not_found()));
        }
        let now = app.clock().now_ms();
        let mut merged = a.clone();
        merged.enabled = enabled;
        let mut text = achievement_summary(&merged, &settings, roles.as_ref(), now);
        if enabled {
            text.push_str(&dependency_warnings(trigger_of(&a), &settings));
            if a.deadline.is_some_and(|d| d.timestamp_millis() <= now) {
                text.push_str("\n⌛ Its deadline has passed, so nothing is counted. Change it with /achievement-config edit.");
            }
        } else {
            text.push_str("\nProgress is kept, and nothing is counted until you turn it back on.");
        }
        Ok(ok_embed(module_off, if enabled { "Achievement Enabled" } else { "Achievement Disabled" }, &text))
    })
    .await?;
    send(ctx, out, false).await
}

/// `announceLine(settings, guild)`: the announcement settings line of `list`.
pub fn announce_line(settings: &bot_db::GuildSettings, channel_exists: impl Fn(&str) -> bool) -> String {
    let mode = settings.achievement_announce_mode.as_str();
    let id = crate::birthday_service::set_id(&settings.achievement_channel_id);
    let mut line = format!("Announcements: {}", mode_label(mode));
    if let (true, Some(id)) = (mode == "CHANNEL", id) {
        line.push_str(&format!(" <#{id}>"));
    }
    if id.is_some_and(|id| !channel_exists(id)) {
        line.push_str(" ⚠️ channel missing");
    }
    format!("{line} · Format: {}", if settings.achievement_use_embed { "embed" } else { "plain text" })
}

fn channel_cached(app: &App, guild_id: GuildId, id: &str) -> bool {
    id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|v| bot_core::interaction_helpers::cached_channel(app, guild_id, ChannelId::new(v)).is_some())
}

async fn list(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let rows = svc::list_achievements(c.app.db(), &c.g).await?;
    if rows.is_empty() {
        let e = apply_guild_color(info_embed("Achievements", Some("No achievements yet. Try `/achievement-config preset` or `/achievement-config create`."), None), Some(&c.settings));
        ctx.edit_reply(Payload::embed(e).to_edit()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(());
    }
    let now = c.app.clock().now_ms();
    let lines: Vec<String> = rows
        .iter()
        .map(|a| {
            let goals: Vec<String> = normalize_tiers(&a.tiers).iter().map(|t| format_number(t.goal as f64)).collect();
            let status = match achievement_status(a.enabled, a.deadline.map(|d| d.timestamp_millis()), now) {
                "on" => "✅ on",
                "off" => "⏸️ off",
                _ => "⌛ ended",
            };
            let rc = reward_count_of(a);
            let gift = if rc > 0 { format!(" · 🎁 {rc}") } else { String::new() };
            format!("{} **{}** · {} · {} · {status}{gift}", badge_of(a.emoji.as_deref(), trigger_of(a)), a.name, trigger_label(trigger_of(a)).unwrap_or_default(), goals.join(" → "))
        })
        .collect();
    let mut pages = Vec::new();
    for (i, chunk) in lines.chunks(LIST_PAGE_SIZE).enumerate() {
        let mut body = format!("{}/{} achievements\n\n{}", rows.len(), bot_config::limits::ACHIEVEMENTS, chunk.join("\n"));
        if i == 0 {
            body.push_str(&format!("\n\n{}", announce_line(&c.settings, |id| channel_cached(&c.app, c.guild_id, id))));
        }
        pages.push(apply_guild_color(info_embed("Achievements", Some(&clamp_text(&body, 4096)), None), Some(&c.settings)));
    }
    paginate(ctx, &pages, PaginateOptions::default()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

async fn info(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let Some(a) = svc::resolve_achievement(c.app.db(), &c.g, &ctx.get_string("achievement").unwrap_or_default()).await? else {
        return send(ctx, Err(not_found()), false).await;
    };
    let tiers = normalize_tiers(&a.tiers);
    let (per_tier, total) = svc::get_tier_stats(c.app.db(), &a.id, tiers.len()).await?;
    let trigger = trigger_of(&a);
    let now = c.app.clock().now_ms();
    let mut header: Vec<String> = Vec::new();
    if let Some(d) = a.description.as_deref().filter(|d| !d.is_empty()) {
        header.push(format!("*{d}*"));
    }
    header.push(format!("Trigger: {} · Status: {}", trigger_label(trigger).unwrap_or_default(), achievement_status(a.enabled, a.deadline.map(|d| d.timestamp_millis()), now)));
    header.push(format!("Past activity: {} · Deadline: {}", if a.track_past_progress { "counted" } else { "not counted" }, deadline_text(a.deadline)));
    header.push(format!("Members with progress: {}", format_number(total as f64)));
    let mut embed = info_embed(&format!("{} {}", badge_of(a.emoji.as_deref(), trigger), a.name), Some(&header.join("\n")), None);
    for (i, t) in tiers.iter().enumerate() {
        let rewards = format_rewards(&t.rewards, c.roles.as_ref(), Some(&c.settings));
        let rewards = if rewards.is_empty() { "No rewards".to_string() } else { rewards };
        embed = embed.field(
            clamp_text(&format!("{} {} — {}", TIER_EMOJIS[i], TIER_NAMES[i], goal_text(trigger, t.goal)), 256),
            clamp_text(&format!("{rewards}\nUnlocked by {} member(s)", format_number(per_tier[i] as f64)), 1024),
            false,
        );
    }
    let embed = apply_guild_color(embed, Some(&c.settings));
    ctx.edit_reply(Payload::embed(embed).no_mentions().to_edit()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

async fn reset(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let Some(user_id) = ctx.get_user_id("member") else { anyhow::bail!("Required option \"member\" not found.") };
    if ctx.get_user("member").is_some_and(|u| u.bot) {
        return send(ctx, Err(fail("Not Available", "Bots don't earn achievements.")), false).await;
    }
    let scope: Vec<Achievement> = match ctx.get_string("achievement").filter(|s| !s.is_empty()) {
        Some(input) => match svc::resolve_achievement(c.app.db(), &c.g, &input).await? {
            Some(a) => vec![a],
            None => return send(ctx, Err(not_found()), false).await,
        },
        None => {
            let all = svc::list_achievements(c.app.db(), &c.g).await?;
            if all.is_empty() {
                ctx.edit_reply(Payload::embed(info_embed("Nothing To Reset", Some("This server has no achievements."), None)).to_edit()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                return Ok(());
            }
            all
        }
    };
    let label = if scope.len() == 1 { format!("**{}**", scope[0].name) } else { format!("all {} achievements", scope.len()) };
    let yes = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Reset achievement progress?".into()),
            description: Some(format!("Reset <@{user_id}>'s progress on {label}? Roles, XP and coins already given are **kept**.")),
            confirm_label: "Reset".into(),
            ..Default::default()
        },
    )
    .await;
    if !yes {
        return Ok(());
    }
    // A config write (serialised with the others of this guild) that does not change the index.
    let ids: Vec<String> = scope.iter().map(|a| a.id.clone()).collect();
    let (app, g, uid) = (c.app.clone(), c.g.clone(), user_id.to_string());
    with_config_lock(&c.app, &c.g, move || async move { svc::reset_member_progress(&app, &g, &uid, &ids).await }).await?;
    let level_note = if scope.iter().any(|a| trigger_of(a) == "LEVEL") { "\nℹ️ Level achievements unlock again when the member next levels up." } else { "" };
    let text = format!("<@{user_id}>'s progress on {label} was reset to zero. Rewards already given were kept.{level_note}");
    send(ctx, ok_embed(c.module_off, "Progress Reset", &text), true).await
}

async fn preset(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let Some(p) = svc::preset(&ctx.get_string("template").unwrap_or_default()) else {
        return send(ctx, Err(fail("Unknown Template", "Pick one of the pre-made achievements.")), false).await;
    };
    let Some(name) = normalize_achievement_name(Some(&ctx.get_string("name").unwrap_or_else(|| p.name.to_string()))) else {
        return send(ctx, Err(fail("Invalid Name", "Names must be 1-50 characters.")), false).await;
    };
    let data = NewAchievement {
        name: name.clone(),
        description: Some(p.description.into()),
        emoji: Some(p.emoji.into()),
        trigger: p.trigger.into(),
        track_past_progress: true,
        deadline: None,
        tiers: tier_inputs_json(&p.goals.iter().map(|g| (*g, Vec::new())).collect::<Vec<_>>()),
    };
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let row = match svc::create_achievement(&app, &g, &data).await? {
            Ok(r) => r,
            Err(r) => return Ok(Err(creation_refusal(r, &name, true))),
        };
        let text = format!(
            "{}\nℹ️ Add rewards with `/achievement-config tier edit`.{}",
            achievement_summary(&row, &settings, roles.as_ref(), app.clock().now_ms()),
            dependency_warnings(p.trigger, &settings)
        );
        Ok(ok_embed(module_off, "Achievement Added", &text))
    })
    .await?;
    send(ctx, out, false).await
}

fn with_tiers(a: &Achievement, tiers: &[Tier]) -> Achievement {
    let mut m = a.clone();
    m.tiers = tiers_json(tiers);
    m
}

fn goal_cap_error(trigger: &str) -> Fail {
    let max = goal_max(trigger).unwrap_or(GOAL_INPUT_MAX);
    fail("Invalid Goal", format!("{} goals can be at most {}.", trigger_label(trigger).unwrap_or_default(), format_number(max as f64)))
}

async fn tier_add(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let goal = ctx.get_integer("goal").unwrap_or(0);
    let input = ctx.get_string("achievement").unwrap_or_default();
    let rewards = collect_rewards(ctx, c);
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let Some(a) = svc::resolve_achievement(app.db(), &g, &input).await? else { return Ok(Err(not_found())) };
        let tiers = normalize_tiers(&a.tiers);
        let max_tiers = bot_config::limits::ACHIEVEMENT_TIERS;
        if tiers.len() >= max_tiers {
            return Ok(Err(fail("Too Many Tiers", format!("An achievement can have at most {max_tiers} tiers."))));
        }
        let Some(last) = tiers.last().map(|t| t.goal) else { anyhow::bail!("Cannot read properties of undefined (reading 'goal')") };
        if goal <= last {
            return Ok(Err(fail("Invalid Goal", format!("The new goal must be higher than the top tier's goal ({}).", format_number(last as f64)))));
        }
        let trigger = trigger_of(&a);
        if goal > goal_max(trigger).unwrap_or(GOAL_INPUT_MAX) {
            return Ok(Err(goal_cap_error(trigger)));
        }
        let (rw, _) = match rewards {
            Ok(r) => r,
            Err(f) => return Ok(Err(f)),
        };
        let mut inputs: Vec<(i64, Vec<Reward>)> = tiers.iter().map(|t| (t.goal, t.rewards.clone())).collect();
        inputs.push((goal, rw));
        let next = serialize_tiers(&tier_inputs_json(&inputs));
        if let Err(e) = validate_tiers(&tiers_json(&next), trigger) {
            return Ok(Err(fail("Invalid Goal", e)));
        }
        svc::apply_tier_change(app.db(), &a.id, &next, TierChange::Add).await?;
        let n = next.len() - 1;
        let text = format!(
            "{} **{}** added to **{}**.\n\n{}\nMembers who already passed this goal unlock it with their next activity.",
            TIER_EMOJIS[n],
            TIER_NAMES[n],
            a.name,
            achievement_summary(&with_tiers(&a, &next), &settings, roles.as_ref(), app.clock().now_ms())
        );
        Ok(ok_embed(module_off, "Tier Added", &text))
    })
    .await?;
    send(ctx, out, false).await
}

async fn tier_edit(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let tier_num = ctx.get_integer("tier").unwrap_or(1);
    let goal = ctx.get_integer("goal");
    let clear = ctx.get_boolean("clear_rewards") == Some(true);
    let reward_given = ["role_add", "role_remove", "xp", "coins"].iter().any(|n| ctx.has_option(n));
    let input = ctx.get_string("achievement").unwrap_or_default();
    let rewards = collect_rewards(ctx, c);
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let Some(a) = svc::resolve_achievement(app.db(), &g, &input).await? else { return Ok(Err(not_found())) };
        let tiers = normalize_tiers(&a.tiers);
        let k = (tier_num - 1).max(0) as usize;
        if k >= tiers.len() {
            return Ok(Err(fail("Unknown Tier", format!("**{}** has only {} tier(s).", a.name, tiers.len()))));
        }
        if goal.is_none() && !reward_given && !clear {
            return Ok(Err(fail("Nothing To Change", "Pass at least one option to change.")));
        }
        let trigger = trigger_of(&a);
        if let Some(goal) = goal {
            if goal > goal_max(trigger).unwrap_or(GOAL_INPUT_MAX) {
                return Ok(Err(goal_cap_error(trigger)));
            }
            let prev = if k > 0 { tiers[k - 1].goal } else { 0 };
            let next_goal = tiers.get(k + 1).map(|t| t.goal);
            if !(goal > prev && next_goal.is_none_or(|n| goal < n)) {
                let below = next_goal.map(|n| format!(" and below {}", format_number(n as f64))).unwrap_or_default();
                return Ok(Err(fail("Invalid Goal", format!("Tier {tier_num} must be above {}{below}.", format_number(prev as f64)))));
            }
        }
        let (rw, given) = match rewards {
            Ok(r) => r,
            Err(f) => return Ok(Err(f)),
        };
        let base: &[Reward] = if clear { &[] } else { &tiers[k].rewards };
        let merged = merge_rewards(base, &rw, &given);
        let inputs: Vec<(i64, Vec<Reward>)> =
            tiers.iter().enumerate().map(|(i, t)| if i == k { (goal.unwrap_or(t.goal), merged.clone()) } else { (t.goal, t.rewards.clone()) }).collect();
        let next = serialize_tiers(&tier_inputs_json(&inputs));
        if let Err(e) = validate_tiers(&tiers_json(&next), trigger) {
            return Ok(Err(fail("Invalid Goal", e)));
        }
        let goal_changed = goal.is_some_and(|g| g != tiers[k].goal);
        svc::apply_tier_change(app.db(), &a.id, &next, TierChange::Edit { index: k, goal_changed }).await?;
        let mut text = achievement_summary(&with_tiers(&a, &next), &settings, roles.as_ref(), app.clock().now_ms());
        if goal_changed {
            text.push_str(if goal.unwrap_or(0) < tiers[k].goal {
                "\nMembers who already passed the new goal unlock it with their next activity."
            } else {
                "\nMembers who already unlocked this tier keep it."
            });
        }
        Ok(ok_embed(module_off, "Tier Updated", &text))
    })
    .await?;
    send(ctx, out, false).await
}

async fn tier_remove(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let tier_num = ctx.get_integer("tier").unwrap_or(1);
    let input = ctx.get_string("achievement").unwrap_or_default();
    let (app, g, settings, roles, module_off) = (c.app.clone(), c.g.clone(), c.settings.clone(), c.roles.clone(), c.module_off);
    let out = locked(c, true, move || async move {
        let Some(a) = svc::resolve_achievement(app.db(), &g, &input).await? else { return Ok(Err(not_found())) };
        let tiers = normalize_tiers(&a.tiers);
        let k = (tier_num - 1).max(0) as usize;
        if k >= tiers.len() {
            return Ok(Err(fail("Unknown Tier", format!("**{}** has only {} tier(s).", a.name, tiers.len()))));
        }
        if tiers.len() == 1 {
            return Ok(Err(fail("Last Tier", "An achievement needs at least one tier. Delete it with `/achievement-config delete` instead.")));
        }
        let inputs: Vec<(i64, Vec<Reward>)> = tiers.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, t)| (t.goal, t.rewards.clone())).collect();
        let next = serialize_tiers(&tier_inputs_json(&inputs));
        svc::apply_tier_change(app.db(), &a.id, &next, TierChange::Remove { index: k }).await?;
        let text = format!(
            "{}\nRewards already given for that tier were kept.",
            achievement_summary(&with_tiers(&a, &next), &settings, roles.as_ref(), app.clock().now_ms())
        );
        Ok(ok_embed(module_off, "Tier Removed", &text))
    })
    .await?;
    send(ctx, out, false).await
}

/// `channelProblem(chan, embedOn)`: the bot must be able to post in `chan`; embed mode also needs EmbedLinks.
fn channel_problem(c: &C, chan: Option<ChannelId>, embed_on: bool) -> Option<Fail> {
    let needs: &[&str] = if embed_on { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] };
    let u = check_channel_usable(&c.app, c.guild_id, chan, needs);
    if u.ok {
        return None;
    }
    let mention = chan.map(|c| format!("<#{c}>")).unwrap_or_else(|| "null".into());
    Some(if u.missing.is_empty() {
        fail("Missing Permissions", u.reason.unwrap_or_default())
    } else {
        fail("Missing Permissions", format!("I need **{}** in {mention} to announce there.", permission_names(&u.missing).join(", ")))
    })
}

async fn save(c: &C, patch: SettingsPatch) -> anyhow::Result<()> {
    c.app.settings().update_settings(&c.g, patch).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

async fn announce_mode(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let mode = ctx.get_string("mode").unwrap_or_default();
    let opt_channel = ctx.get_channel_id("channel");
    let embed = ctx.get_boolean("embed");
    // A stored id that is no longer in the cache counts as no channel (display only; nothing is deleted).
    let stored = crate::birthday_service::set_id(&c.settings.achievement_channel_id).filter(|id| channel_cached(&c.app, c.guild_id, id)).and_then(|id| id.parse::<u64>().ok()).map(ChannelId::new);
    let chan = opt_channel.or(stored);
    if mode == "CHANNEL" && chan.is_none() {
        return send(ctx, Err(fail("Pick a Channel", "Custom channel mode needs a channel. Run this again with `channel:`.")), false).await;
    }
    let embed_on = embed.unwrap_or(c.settings.achievement_use_embed);
    if (mode == "CHANNEL" || opt_channel.is_some())
        && let Some(p) = channel_problem(c, chan, embed_on)
    {
        return send(ctx, Err(p), false).await;
    }
    let mut patch = SettingsPatch::new().set("achievementAnnounceMode", mode.clone());
    if let Some(ch) = opt_channel {
        patch = patch.set("achievementChannelId", ch.to_string());
    }
    if let Some(e) = embed {
        patch = patch.set("achievementUseEmbed", e);
    }
    save(c, patch).await?;
    let channel_id = opt_channel.map(|c| c.to_string()).or_else(|| crate::birthday_service::set_id(&c.settings.achievement_channel_id).map(str::to_string));
    let mut line = match mode.as_str() {
        "OFF" => "Unlocks are no longer announced.".to_string(),
        "CURRENT" => {
            let mut l = "Unlocks are announced in the channel where they happen.".to_string();
            l.push_str(&match &channel_id {
                Some(id) => format!("\nEvents without a channel (boosts, purchases) use <#{id}>."),
                None => "\nEvents without a channel (boosts, purchases) are not announced unless you set `/achievement-config announce channel`.".to_string(),
            });
            l
        }
        "DM" => "Members get a private message when they unlock a tier, and one \"almost there\" message at 90% of their next goal.\nMembers who block DMs from this server get nothing.".to_string(),
        _ => format!("Unlocks are announced in <#{}>.", channel_id.as_deref().unwrap_or("null")),
    };
    if let Some(e) = embed {
        let what = if e { "an embed" } else { "plain text" };
        if mode != "OFF" {
            line.push_str(&format!("\nUnlocks are sent as {what}."));
        } else {
            line.push_str(&format!("\nWhen turned on again, unlocks are sent as {what}."));
        }
    }
    send(ctx, ok_embed(c.module_off, "Announcements Updated", &line), false).await
}

async fn announce_channel(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let Some(channel) = ctx.get_channel_id("channel") else { anyhow::bail!("Required option \"channel\" not found.") };
    if let Some(p) = channel_problem(c, Some(channel), c.settings.achievement_use_embed) {
        return send(ctx, Err(p), false).await;
    }
    let old_mode = c.settings.achievement_announce_mode.as_str();
    save(c, SettingsPatch::new().set("achievementChannelId", channel.to_string()).set("achievementAnnounceMode", "CHANNEL")).await?;
    let switched = if old_mode != "CHANNEL" { format!("\nMode switched from {} to Custom channel.", mode_label(old_mode)) } else { String::new() };
    send(ctx, ok_embed(c.module_off, "Announcements Updated", &format!("Unlocks are announced in <#{channel}>.{switched}")), false).await
}

async fn announce_message(ctx: &CommandCtx, c: &C) -> anyhow::Result<()> {
    let text = ctx.get_string("text").map(|t| bot_util::js::trim(&t).to_string()).unwrap_or_default();
    if text.is_empty() {
        save(c, SettingsPatch::new().set("achievementMessage", serde_json::Value::Null)).await?;
        return send(ctx, ok_embed(c.module_off, "Announcement Message Reset", &format!("Using the default: {DEFAULT_ACHIEVEMENT_MESSAGE}")), false).await;
    }
    let member = ctx.member();
    let display = member.map(bot_core::adapters::member_display_name);
    let mut rctx = bot_core::adapters::render_context(&c.app, Some(c.guild_id), ctx.user(), member, Some(ctx.channel_id()));
    rctx.vars = Some(svc::to_vars(svc::sample_vars(member.map(|m| m.user.id.to_string()).as_deref(), display.as_deref())));
    let d = bot_util::placeholder_parser::render_placeholders_detailed(&text, &rctx, Some(2000));
    if bot_util::js::trim(&d.text).is_empty() {
        return send(ctx, Err(fail("Empty Message", "That text renders as an empty message.")), false).await;
    }
    save(c, SettingsPatch::new().set("achievementMessage", text.clone())).await?;
    let off = if c.settings.achievement_announce_mode.as_str() == "OFF" {
        "\nℹ️ Announcements are disabled; turn them on with /achievement-config announce mode."
    } else {
        ""
    };
    let mut out = ok_embed(c.module_off, "Announcement Message Updated", &format!("Preview below.{off}"));
    if let Ok(e) = out {
        let mut e = e.field("Preview", clamp_text(&d.text, 1024), false);
        if !d.unknown_tokens.is_empty() {
            let toks: Vec<String> = d.unknown_tokens.iter().map(|t| format!("`{t}`")).collect();
            e = e.field("Unknown variables", join_with_limit(&toks, ", ", 1024, bot_util::text::default_more), false);
        }
        out = Ok(e);
    }
    send(ctx, out, false).await
}

fn guild_roles(app: &App, guild_id: GuildId) -> Option<HashSet<String>> {
    let g = bot_core::interaction_helpers::cached_guild(app, guild_id)?;
    Some(g.roles.keys().map(|r| r.to_string()).collect())
}

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    let r: anyhow::Result<()> = async {
        // delete and reset prompt for confirmation, so their defer is ephemeral; the result is posted publicly afterwards.
        let ephemeral = key == "delete" || key == "reset";
        if !safe_defer(&ctx.responder, ephemeral).await {
            return Ok(());
        }
        let required: &[&str] = if key == "announce channel" { &["channel"] } else { &[] };
        if !resolve_new_options(&mut ctx, NewOptions { required, private: &[] }).await {
            return Ok(());
        }
        let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/achievement-config outside a guild"))?;
        let g = guild_id.to_string();
        let settings = ctx.app.settings().get_settings(&g).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let module_off = !is_module_enabled(Some(&settings), Some(ModuleKey::Achievements));
        let c = C { app: ctx.app.clone(), guild_id, g, roles: guild_roles(&ctx.app, guild_id), settings, module_off };
        match key.as_str() {
            "create" => create(&ctx, &c).await,
            "edit" => edit(&ctx, &c).await,
            "delete" => remove(&ctx, &c).await,
            "enable" => toggle(&ctx, &c, true).await,
            "disable" => toggle(&ctx, &c, false).await,
            "list" => list(&ctx, &c).await,
            "info" => info(&ctx, &c).await,
            "reset" => reset(&ctx, &c).await,
            "preset" => preset(&ctx, &c).await,
            "tier add" => tier_add(&ctx, &c).await,
            "tier edit" => tier_edit(&ctx, &c).await,
            "tier remove" => tier_remove(&ctx, &c).await,
            "announce mode" => announce_mode(&ctx, &c).await,
            "announce channel" => announce_channel(&ctx, &c).await,
            "announce message" => announce_message(&ctx, &c).await,
            _ => Ok(()),
        }
    }
    .await;
    if let Err(e) = r {
        bot_util::log_error!(format!("achievement-config {key} failed"), bot_core::error::log_arg(&e));
        safe_reply(&ctx.responder, Payload::embed(error_embed(GENERIC_ERROR.0, Some(GENERIC_ERROR.1), None)).ephemeral(true)).await;
    }
    Ok(())
}

/// The autocomplete choice of one achievement: `{emoji} {name} · {trigger label}{ (off)}`.
pub fn choice_name(emoji: Option<&str>, name: &str, trigger: &str, enabled: bool) -> String {
    format!("{} {name} · {}{}", emoji.unwrap_or(trigger_emoji(trigger)), trigger_label(trigger).unwrap_or_default(), if enabled { "" } else { " (off)" })
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    if name != "achievement" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let text = bot_util::js::utf16_prefix(bot_util::js::trim(&value), 50).to_string();
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    let mut rows = Vec::new();
    if budget > 0
        && let Some(g) = ctx.guild_id()
    {
        let (app, gs, t) = (ctx.app.clone(), g.to_string(), text.clone());
        let fut = async move { Ok::<_, std::convert::Infallible>(svc::search_achievements(app.db(), &gs, &t).await) };
        rows = bot_util::async_util::with_timeout(fut, budget, "achievement autocomplete").await.unwrap_or_default();
    }
    let choices: Vec<Choice> = rows.iter().map(|a| Choice::new(choice_name(a.emoji.as_deref(), &a.name, &a.trigger, a.enabled), a.id.clone())).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
