//! Port of `src/commands/admin/rolepersist.js`: `/rolepersist status | ignore add|remove | view | clear`. An admin
//! config command: replies are public and it keeps working while the Role Persistence module is off
//! (`gated = false`). The hooks are [`crate::plugins::role_persist`].

use bot_core::command::CommandOption as Opt;
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Payload, PermTarget, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_core::settings::is_module_enabled;
use bot_util::text::{format_number, join_with_limit};
use serenity::all::{GuildId, Permissions};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};

use crate::role_persist_service::{clear_waiting, count_rows, delete_row, expires_at, is_stale, load_row, plan_roles, retention_days};

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("rolepersist", "Give members their roles back when they leave and rejoin.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("status", "Show the setup and how many members are saved."))
        .option(
            Opt::sub_command_group("ignore", "Roles that are never given back.")
                .option(Opt::sub_command("add", "Never give this role back.").option(Opt::role("role", "The role.").required(true)))
                .option(Opt::sub_command("remove", "Give this role back again.").option(Opt::role("role", "The role.").required(true))),
        )
        .option(Opt::sub_command("view", "Show what is saved for a member.").option(Opt::user("user", "The member who left.").required(true)))
        .option(Opt::sub_command("clear", "Forget what is saved for a member.").option(Opt::user("user", "The member who left.").required(true)))
}

/// The command (`module 'rolepersist'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Rolepersist), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}
fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}
fn lookup_failed() -> CreateEmbed {
    error_embed("Lookup Failed", Some("The saved roles could not be read. Please try again in a moment."), None)
}
fn everyone() -> CreateEmbed {
    error_embed("Invalid Role", Some("@everyone is never saved or given back, so it cannot be on this list."), None)
}
fn managed() -> CreateEmbed {
    info_embed("Not Needed", Some("That role is managed by an integration. Managed roles are never saved or given back."), None)
}
fn full() -> CreateEmbed {
    error_embed(
        "List Full",
        Some(&format!(
            "At most {} roles can be on this list. Remove one with `/rolepersist ignore remove` first.",
            bot_config::limits::ROLEPERSIST_IGNORED_ROLES
        )),
        None,
    )
}

/// `OFF_NOTE`.
pub const OFF_NOTE: &str = "\n\n⚠️ The **Role Persistence** module is off. Turn it on with `/module enable rolepersist`.";

/// `finish(interaction, embed)`: overwrites the deferred reply; never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn stamp(ms: i64) -> String {
    format!("<t:{}:R>", ms.div_euclid(1000))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    if group.as_deref() == Some("ignore") {
        return run_ignore(ctx, sub.as_deref() == Some("add")).await;
    }
    match sub.as_deref() {
        Some("status") => run_status(ctx).await,
        Some("view") => run_view(ctx).await,
        Some("clear") => run_clear(ctx).await,
        _ => Ok(()),
    }
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/rolepersist outside a guild"))
}

// ── /rolepersist status ───────────────────────────────────────────────────────

/// The status embed's fields: `(name, value, inline)`.
pub fn status_fields(enabled: bool, days: i64, nicknames: bool, saved: &str, ignored: &[String], missing: &[String]) -> Vec<(String, String, bool)> {
    let max = bot_config::limits::ROLEPERSIST_IGNORED_ROLES;
    let mut fields = vec![
        ("Status".to_string(), if enabled { "✅ On".to_string() } else { "❌ Off — `/module enable rolepersist`".to_string() }, true),
        ("Kept for".to_string(), format!("{days} days"), true),
        ("Nicknames".to_string(), if nicknames { "✅ Given back".to_string() } else { "❌ Not given back".to_string() }, true),
        ("Saved members".to_string(), saved.to_string(), true),
        (
            format!("Never given back ({}/{max})", ignored.len()),
            if ignored.is_empty() {
                "None — `/rolepersist ignore add`".to_string()
            } else {
                let mentions: Vec<String> = ignored.iter().map(|id| format!("<@&{id}>")).collect();
                join_with_limit(&mentions, ", ", 1024, bot_util::text::default_more)
            },
            false,
        ),
    ];
    if !missing.is_empty() {
        fields.push(("Problem".to_string(), format!("⚠️ I need **{}**.", bot_core::permissions::permission_names(missing).join(", ")), false));
    }
    fields
}

async fn run_status(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };
    let enabled = is_module_enabled(Some(&settings), Some(ModuleKey::Rolepersist));
    let saved = match count_rows(&ctx.app, &g).await {
        Ok(n) => format_number(n as f64),
        Err(e) => {
            bot_util::log_warn!(format!("[role-persist] {g}: could not count saved members: {e}"));
            "Unavailable".to_string()
        }
    };
    let mut needs = vec!["ManageRoles"];
    if settings.role_persist_nicknames {
        needs.push("ManageNicknames");
    }
    let missing = missing_bot_permissions(&ctx.app, PermTarget::Guild(guild_id), &needs);
    let mut embed = guild_embed(Some(&settings), Some("🔁 Role Persistence"), None, None);
    for (n, v, i) in status_fields(enabled, retention_days(Some(&settings)), settings.role_persist_nicknames, &saved, &settings.role_persist_ignored_role_ids, &missing) {
        embed = embed.field(n, v, i);
    }
    embed = embed.footer(CreateEmbedFooter::new(
        "Roles with powerful permissions, managed roles and roles above mine are never given back. Days and nicknames are set on the dashboard.",
    ));
    finish(&ctx, embed).await
}

// ── /rolepersist ignore add | remove ──────────────────────────────────────────

#[derive(Debug)]
enum Outcome {
    Same,
    Full,
    Changed,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Outcome {}

async fn run_ignore(ctx: CommandCtx, adding: bool) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let role_id = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let cached = bot_core::interaction_helpers::with_cached_guild(&ctx.app, guild_id, |guild| guild.roles.get(&role_id).map(|r| r.managed)).flatten();
    let is_managed = cached.or_else(|| ctx.get_role("role").map(|r| r.managed)).unwrap_or(false);
    if role_id.get() == guild_id.get() {
        return finish(&ctx, everyone()).await;
    }
    if adding && is_managed {
        return finish(&ctx, managed()).await;
    }
    let role = role_id.to_string();
    let r2 = role.clone();
    let res = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let list = cur.role_persist_ignored_role_ids.clone();
            let has = list.contains(&r2);
            if adding {
                if has {
                    anyhow::bail!(Outcome::Same);
                }
                if list.len() >= bot_config::limits::ROLEPERSIST_IGNORED_ROLES {
                    anyhow::bail!(Outcome::Full);
                }
                let mut next = list;
                next.push(r2);
                return Ok(SettingsPatch::new().set("rolePersistIgnoredRoleIds", serde_json::json!(next)));
            }
            if !has {
                anyhow::bail!(Outcome::Same);
            }
            let next: Vec<String> = list.into_iter().filter(|id| *id != r2).collect();
            Ok(SettingsPatch::new().set("rolePersistIgnoredRoleIds", serde_json::json!(next)))
        })
        .await;
    let outcome = match res {
        Ok(_) => Outcome::Changed,
        Err(e) => match e.downcast_ref::<Outcome>() {
            Some(Outcome::Same) => Outcome::Same,
            Some(Outcome::Full) => Outcome::Full,
            _ => {
                bot_util::log_error!(
                    format!("[role-persist] {g}: ignore {} failed", if adding { "add" } else { "remove" }),
                    bot_core::error::log_arg(&e)
                );
                return finish(&ctx, could_not_save()).await;
            }
        },
    };
    match outcome {
        Outcome::Full => return finish(&ctx, full()).await,
        Outcome::Same => {
            let text = if adding {
                format!("<@&{role}> is already on the never-give-back list.")
            } else {
                format!("<@&{role}> is not on the never-give-back list.")
            };
            return finish(&ctx, info_embed("No Change", Some(&text), None)).await;
        }
        Outcome::Changed => {}
    }
    bot_util::log_info!(format!("[role-persist] {g}: {} {role} by {}", if adding { "ignore add" } else { "ignore remove" }, ctx.user().id));
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let off = if settings.as_ref().is_some_and(|s| !is_module_enabled(Some(s), Some(ModuleKey::Rolepersist))) { OFF_NOTE } else { "" };
    let (title, text) = if adding {
        ("Role Ignored", format!("<@&{role}> is never given back, even to members who already left with it."))
    } else {
        ("Role No Longer Ignored", format!("<@&{role}> is given back again, including to members who already left with it."))
    };
    finish(&ctx, success_embed(title, Some(&format!("{text}{off}")), None)).await
}

// ── /rolepersist view ─────────────────────────────────────────────────────────

async fn run_view(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let user = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };
    let row = match load_row(&ctx.app, &g, &user.to_string()).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!(format!("[role-persist] {g}: could not read {user}: {e}"));
            return finish(&ctx, lookup_failed()).await;
        }
    };
    let Some(row) = row else {
        let text = format!("Nothing is saved for <@{user}>. Roles are saved when a member leaves and dropped after {} days.", retention_days(Some(&settings)));
        return finish(&ctx, info_embed("Nothing Saved", Some(&text), None)).await;
    };
    let bot = ctx.app.discord().map(|d| d.bot_user_id());
    let plan = bot
        .and_then(|b| bot_core::interaction_helpers::with_cached_guild(&ctx.app, guild_id, |guild| plan_roles(guild, b, &row.role_ids, Some(&settings), None, None)))
        .unwrap_or_else(|| crate::role_persist_service::Plan {
            give: Vec::new(),
            skipped: row
                .role_ids
                .iter()
                .map(|id| crate::role_persist_service::Skipped { role_id: id.clone(), reason: "The role no longer exists.".into(), deleted: true })
                .collect(),
        });
    let mut lines: Vec<String> = plan.give.iter().map(|r| format!("✅ <@&{r}>")).collect();
    for s in &plan.skipped {
        lines.push(if s.deleted { format!("❌ deleted role (`{}`)", s.role_id) } else { format!("⚠️ <@&{}> — {}", s.role_id, s.reason) });
    }
    let now = ctx.app.clock().now_ms();
    let stale = is_stale(&row.left_at, Some(&settings), now);
    let nickname = match row.nickname.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!("`{n}`{}", if settings.role_persist_nicknames { "" } else { " (nicknames are off)" }),
        None => "None".into(),
    };
    let embed = guild_embed(Some(&settings), Some("🔁 Saved Roles"), None, None)
        .field("Member", format!("<@{user}>"), true)
        .field("Left", stamp(row.left_at.timestamp_millis()), true)
        .field(
            if stale { "Expired" } else { "Expires" },
            if stale { "Dropped on the next sweep".to_string() } else { stamp(expires_at(&row.left_at, Some(&settings))) },
            true,
        )
        .field("Roles", if lines.is_empty() { "None".to_string() } else { join_with_limit(&lines, "\n", 1024, bot_util::text::default_more) }, false)
        .field("Nickname", nickname, true)
        .footer(CreateEmbedFooter::new("✅ given back on rejoin · ⚠️ skipped"));
    finish(&ctx, embed).await
}

// ── /rolepersist clear ────────────────────────────────────────────────────────

async fn run_clear(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let user = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
    let removed = match delete_row(&ctx.app, &g, &user.to_string()).await {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_error!(format!("[role-persist] {g}: clear failed for {user}"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    clear_waiting(&ctx.app, &g, &user.to_string());
    if removed == 0 {
        return finish(&ctx, info_embed("Nothing Saved", Some(&format!("Nothing was saved for <@{user}>.")), None)).await;
    }
    bot_util::log_info!(format!("[role-persist] {g}: cleared {user} by {}", ctx.user().id));
    finish(&ctx, success_embed("Cleared", Some(&format!("<@{user}> will not get their old roles back when they rejoin.")), None)).await
}
