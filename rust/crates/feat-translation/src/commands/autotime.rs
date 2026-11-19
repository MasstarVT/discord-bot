//! Port of `src/commands/admin/autotime.js`: `/autotime enable | disable | status | ignore add | ignore remove`. An
//! admin config command: replies are public and it keeps working while the Auto Timestamps module is off
//! (`gated = false`). The answering itself is [`crate::plugins::autotime`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Payload, cached_channel, cached_guild, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::timezones::normalize_timezone_roles;
use serenity::all::{ChannelType, Guild, Permissions, RoleId};
use serenity::builder::CreateEmbed;

const MAX_IGNORED: usize = bot_config::tables::limits::AUTOTIME_IGNORED_CHANNELS;
const IGNORABLE_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::GUILD_FORUM,
    channel_type::GUILD_MEDIA,
    channel_type::GUILD_CATEGORY,
];

/// `HOW_IT_WORKS`.
pub const HOW_IT_WORKS: &str = "When a member types a time such as `fri 5pm`, `tomorrow at 18:30` or `7pm EST`, I reply with a Discord timestamp that everyone sees in their own timezone. Times are read in the timezone of the member who typed them: their own (`/timezone set`) or their timezone role. Members with neither are only answered when they name the zone (`7pm EST`); otherwise I tell them to pick a timezone role or use `/timezone set`.";

const OFF_NOTE: &str = "\n\n⚠️ The **Auto Timestamps** module is off. Turn it on with `/autotime enable`.";

/// The command's data.
pub fn data() -> SlashCommand {
    let channel = |desc: &str| Opt::channel("channel", desc).required(true).channel_types(&IGNORABLE_TYPES);
    SlashCommand::new("autotime", "Answer times typed in chat (\"fri 5pm\") with a timestamp everyone sees in their own timezone.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("enable", "Turn Auto Timestamps on."))
        .option(Opt::sub_command("disable", "Turn Auto Timestamps off."))
        .option(Opt::sub_command("status", "Show who is answered and the ignored channels."))
        .option(
            Opt::sub_command_group("ignore", "Channels where times are never answered.")
                .option(Opt::sub_command("add", "Never answer times in a channel (a category covers its channels).").option(channel("Channel or category.")))
                .option(Opt::sub_command("remove", "Answer times in a channel again.").option(channel("Channel or category."))),
        )
}

/// The command (`module 'autotime'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Autotime), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

/// Final answer: overwrites the deferred reply. Never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

/// `timezoneRolesLine(settings, guild)`: how many timezone roles are linked (and still exist), or how to set them up.
pub fn timezone_roles_line(settings: Option<&GuildSettings>, guild: Option<&Guild>) -> String {
    let null = serde_json::Value::Null;
    let count = normalize_timezone_roles(settings.and_then(|s| s.timezone_roles.as_ref()).unwrap_or(&null))
        .iter()
        .filter(|l| guild.is_some_and(|g| l.role_id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|r| g.roles.contains_key(&RoleId::new(r)))))
        .count();
    if count > 0 {
        return format!("**{count}** timezone role{} linked (`/settings timezone-role list`).", if count == 1 { " is" } else { "s are" });
    }
    "No timezone roles are linked yet. `/reactionrole create template:timezones-us` (or `timezones-world`) posts a panel that creates and links them, or link roles you have with `/settings timezone-role add`.".to_string()
}

fn guild_of(ctx: &CommandCtx) -> Option<Guild> {
    ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g))
}

// ── /autotime enable | disable ────────────────────────────────────────────────

async fn run_toggle(ctx: CommandCtx, on: bool) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autotime outside a guild"))?.to_string();
    let settings = match ctx.app.settings().update_settings(&g, SettingsPatch::new().set("autoTimeEnabled", on)).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[autotime] {g}: {} failed", if on { "enable" } else { "disable" }), e.to_string());
            return finish(&ctx, could_not_save()).await;
        }
    };
    if !on {
        return finish(&ctx, success_embed("Auto Timestamps Off", Some("Times typed in chat are no longer answered. The ignored channels are kept."), None)).await;
    }
    let guild = guild_of(&ctx);
    let desc = format!("{HOW_IT_WORKS}\n\n{}", timezone_roles_line(Some(&settings), guild.as_ref()));
    finish(&ctx, success_embed("Auto Timestamps On", Some(&desc), None)).await
}

// ── /autotime status ──────────────────────────────────────────────────────────

async fn run_status(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let gid = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autotime outside a guild"))?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&gid.to_string()).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let ids = &settings.auto_time_ignored_channel_ids;
    let ignored = if ids.is_empty() {
        "None".to_string()
    } else {
        let items: Vec<String> = ids
            .iter()
            .map(|id| {
                let exists = id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|c| cached_channel(&ctx.app, gid, serenity::all::ChannelId::new(c)).is_some());
                if exists { format!("<#{id}>") } else { format!("#deleted-channel (`{id}`)") }
            })
            .collect();
        bot_util::text::join_with_limit(&items, ", ", 1000, bot_util::text::default_more)
    };
    let guild = guild_of(&ctx);
    let desc = format!("{HOW_IT_WORKS}\n\n{}", timezone_roles_line(Some(&settings), guild.as_ref()));
    let on = is_module_enabled(Some(&settings), Some(ModuleKey::Autotime));
    let embed = guild_embed(Some(&settings), Some("🕒 Auto Timestamps"), Some(&desc), None)
        .field("Status", if on { "✅ On" } else { "❌ Off (`/autotime enable`)" }, true)
        .field(format!("Ignored channels ({}/{MAX_IGNORED})", ids.len()), ignored, true);
    finish(&ctx, embed).await
}

// ── /autotime ignore add | remove ─────────────────────────────────────────────

#[derive(Debug)]
enum Refused {
    Exists,
    Limit,
    Missing,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Refused {}

async fn run_ignore_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autotime outside a guild"))?.to_string();
    let channel_id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let id = channel_id.to_string();
    // The resolved channel (Discord always sends it), else the cached one.
    let (kind, name) = match ctx.get_channel("channel") {
        Some(c) => (c.kind, c.name.clone().unwrap_or_default()),
        None => ctx
            .guild_id()
            .and_then(|g| cached_channel(&ctx.app, g, channel_id))
            .map(|c| (c.kind, c.name.clone()))
            .unwrap_or((ChannelType::Text, String::new())),
    };
    let cid = id.clone();
    let saved = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let list = cur.auto_time_ignored_channel_ids.clone();
            if list.contains(&cid) {
                return Err(Refused::Exists.into());
            }
            if list.len() >= MAX_IGNORED {
                return Err(Refused::Limit.into());
            }
            let mut next = list;
            next.push(cid);
            Ok(SettingsPatch::new().set("autoTimeIgnoredChannelIds", serde_json::json!(next)))
        })
        .await;
    let settings = match saved {
        Ok(s) => s,
        Err(e) => match e.downcast_ref::<Refused>() {
            Some(Refused::Exists) => return finish(&ctx, warn_embed("Already Ignored", Some(&format!("Times in <#{id}> are already ignored.")), None)).await,
            Some(_) => {
                let desc = format!("You can ignore at most {MAX_IGNORED} channels. Remove one with `/autotime ignore remove` first.");
                return finish(&ctx, error_embed("Limit Reached", Some(&desc), None)).await;
            }
            None => {
                bot_util::log_error!(format!("[autotime] {g}: ignore add failed"), e.to_string());
                return finish(&ctx, could_not_save()).await;
            }
        },
    };
    let where_ = if kind == ChannelType::Category {
        format!("any channel of **{name}**")
    } else {
        format!("<#{id}>")
    };
    let off = if is_module_enabled(Some(&settings), Some(ModuleKey::Autotime)) { "" } else { OFF_NOTE };
    finish(&ctx, success_embed("Channel Ignored", Some(&format!("Times typed in {where_} will not be answered.{off}")), None)).await
}

async fn run_ignore_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autotime outside a guild"))?.to_string();
    let id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?.to_string();
    let cid = id.clone();
    let saved = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let list = cur.auto_time_ignored_channel_ids.clone();
            if !list.contains(&cid) {
                return Err(Refused::Missing.into());
            }
            let next: Vec<String> = list.into_iter().filter(|x| *x != cid).collect();
            Ok(SettingsPatch::new().set("autoTimeIgnoredChannelIds", serde_json::json!(next)))
        })
        .await;
    let settings = match saved {
        Ok(s) => s,
        Err(e) => match e.downcast_ref::<Refused>() {
            Some(_) => return finish(&ctx, warn_embed("Not Ignored", Some(&format!("<#{id}> is not on the ignore list.")), None)).await,
            None => {
                bot_util::log_error!(format!("[autotime] {g}: ignore remove failed"), e.to_string());
                return finish(&ctx, could_not_save()).await;
            }
        },
    };
    let off = if is_module_enabled(Some(&settings), Some(ModuleKey::Autotime)) { "" } else { OFF_NOTE };
    finish(&ctx, success_embed("Channel No Longer Ignored", Some(&format!("Times typed in <#{id}> are answered again.{off}")), None)).await
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    if group.as_deref() == Some("ignore") {
        return if sub.as_deref() == Some("add") { run_ignore_add(ctx).await } else { run_ignore_remove(ctx).await };
    }
    match sub.as_deref() {
        Some("enable") => run_toggle(ctx, true).await,
        Some("disable") => run_toggle(ctx, false).await,
        Some("status") => run_status(ctx).await,
        _ => Ok(()),
    }
}
