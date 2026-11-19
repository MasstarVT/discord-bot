//! Port of `src/commands/admin/voice-hub.js`: `/voice-hub` configures the join-to-create hubs (create / edit / delete /
//! list plus the ignore-role and mod-role groups). Validation, replies and calls into
//! [`crate::temp_voice_service`]. Replies are public (admin convention); `delete` defers ephemerally, confirms, then
//! posts the result publicly.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{ConfirmOptions, PermTarget, Payload, await_confirm, cached_channel, cached_guild, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_core::settings::is_module_enabled;
use bot_config::limits;
use bot_db::TempVoiceHub;
use bot_util::text::clamp_text;
use serde_json::json;
use serenity::all::{ChannelId, ChannelType, GuildChannel, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::discord_port::{NewChannel, rest};
use crate::temp_voice_service::{self as tv, HubData, NameContext, NameFormatCheck, RoleListOutcome};

fn add_hub_settings(s: Opt) -> Opt {
    s.option(Opt::channel("category", "Category for new channels (default: the hub's)").channel_types(&[channel_type::GUILD_CATEGORY]))
        .option(new_channel_option("category", "category"))
        .option(Opt::string("name_format", "Name format (default #{index} - {username}'s Channel)").min_length(1).max_length(100))
        .option(Opt::integer("limit", "User limit (0 = no limit)").min_value(0).max_value(99))
        .option(Opt::integer("bitrate", "Bitrate in kbps (capped by boost level)").min_value(8).max_value(384))
        .option(Opt::integer("keep_alive", "Minutes an empty channel is kept (0 = delete at once)").min_value(0).max_value(10080))
        .option(Opt::integer("ownership_lock", "Minutes before an ownerless channel can be claimed").min_value(0).max_value(1440))
        .option(Opt::boolean("text_channel", "Also create a companion text channel"))
        .option(Opt::boolean("text_private", "Text channel only for voice members (default True)"))
        .option(Opt::boolean("sync_permissions", "Copy the category's permissions (False: copy the hub's)"))
}

fn hub_opt(desc: &str) -> Opt {
    Opt::channel("hub", desc).required(true).channel_types(&[channel_type::GUILD_VOICE])
}

fn role_opt() -> Opt {
    Opt::role("role", "Role").required(true)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("voice-hub", "Join-to-create voice hubs (MEE6 Temporary Channels).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(add_hub_settings(
            Opt::sub_command("create", "Create a join-to-create hub (up to 10).")
                .option(Opt::channel("channel", "Voice channel to use as the hub (omit to create one)").channel_types(&[channel_type::GUILD_VOICE]))
                .option(new_channel_option("channel", "voice channel")),
        ))
        .option(add_hub_settings(Opt::sub_command("edit", "Change a hub's settings.").option(hub_opt("Hub to edit"))))
        .option(
            Opt::sub_command("delete", "Delete a hub and its temporary channels.")
                .option(hub_opt("Hub to delete"))
                .option(Opt::boolean("delete_channel", "Also delete the hub voice channel (default False)")),
        )
        .option(Opt::sub_command("list", "List hubs and their settings."))
        .option(
            Opt::sub_command_group("ignore-role", "Roles that /voice commands do not affect")
                .option(Opt::sub_command("add", "Protect a role from /voice commands").option(hub_opt("Hub")).option(role_opt()))
                .option(Opt::sub_command("remove", "Stop protecting a role").option(hub_opt("Hub")).option(role_opt())),
        )
        .option(
            Opt::sub_command_group("mod-role", "Roles that may use /voice on any temporary channel")
                .option(Opt::sub_command("add", "Add a temp-voice moderator role").option(hub_opt("Hub")).option(role_opt()))
                .option(Opt::sub_command("remove", "Remove a temp-voice moderator role").option(hub_opt("Hub")).option(role_opt())),
        )
}

/// The command (`module 'tempvoice'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Tempvoice), execute).gated(false).permission_level(PermissionLevel::Admin)
}

pub const MODULE_OFF: &str = "\n\n⚠️ The Temporary Channels module is off, so hubs do nothing. Turn it on with `/module enable tempvoice`.";
const CREATE_HINT: &str = "`/voice-hub create`";

fn module_note(settings: Option<&bot_db::GuildSettings>) -> &'static str {
    if is_module_enabled(settings, Some(ModuleKey::Tempvoice)) { "" } else { MODULE_OFF }
}

fn shown(name: Option<&str>) -> String {
    escape_markdown(&clamp_text(name.unwrap_or("unknown"), 100))
}

// ── Small formatters (the "old → new" lines of /voice-hub edit) ───────────────

fn limit_text(n: i32) -> String {
    if n > 0 { n.to_string() } else { "none".into() }
}
fn keep_text(n: i32) -> String {
    if n > 0 { format!("{n} min") } else { "delete at once".into() }
}
fn lock_text(n: i32) -> String {
    if n > 0 { format!("{n} min") } else { "none".into() }
}
fn bitrate_text(bps: Option<i32>) -> String {
    match bps.filter(|b| *b != 0) {
        Some(b) => format!("{} kbps", bot_util::js::number_to_string(bot_util::text::js_round(f64::from(b) / 1000.0))),
        None => "default".into(),
    }
}
fn category_text(id: Option<&str>) -> String {
    match id.filter(|c| !c.is_empty()) {
        Some(c) => format!("<#{c}>"),
        None => "hub's category".into(),
    }
}
fn perms_text(sync: bool) -> &'static str {
    if sync { "category" } else { "hub channel" }
}
fn text_mode(h: &TempVoiceHub) -> &'static str {
    if !h.create_text_channel {
        "off"
    } else if h.text_channel_members_only {
        "private"
    } else {
        "public"
    }
}

fn limit_reached() -> CreateEmbed {
    error_embed("Limit Reached", Some(&format!("A server can have at most {} hubs. Delete one with `/voice-hub delete` first.", limits::TEMPVOICE_HUBS)), None)
}
fn not_a_hub(channel: &str) -> CreateEmbed {
    error_embed("Not A Hub", Some(&format!("{channel} is not a hub. See `/voice-hub list`.")), None)
}
fn already_a_hub(channel: &str) -> CreateEmbed {
    error_embed("Already A Hub", Some(&format!("{channel} is already a hub. Change it with `/voice-hub edit`.")), None)
}
fn internal_error() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("I could not save that hub. Please try again in a moment."), None)
}

fn mention(id: ChannelId) -> String {
    format!("<#{id}>")
}

/// `checkNameFormat(nameFormat, interaction)`: `Err(embed)` or `(preview, warnings)`.
fn check_name_format(ctx: &CommandCtx, name_format: Option<&str>) -> Result<(String, Vec<String>), Box<CreateEmbed>> {
    let guild = ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g));
    let name_ctx = match ctx.member() {
        Some(m) => NameContext::of(m, guild.as_ref()),
        None => NameContext::default(),
    };
    match tv::validate_name_format(Some(name_format.unwrap_or(tv::DEFAULT_NAME_FORMAT)), &name_ctx) {
        NameFormatCheck::Err(e) => Err(Box::new(error_embed("Invalid Name Format", Some(&e), None))),
        NameFormatCheck::Ok { preview, warnings } => Ok((preview, if name_format.is_some() { warnings } else { Vec::new() })),
    }
}

/// `permissionError(interaction, targetParent, wantsText)`.
fn permission_error(app: &App, guild_id: GuildId, target_parent: Option<&GuildChannel>, wants_text: bool) -> Option<CreateEmbed> {
    let mut need: Vec<&str> = tv::REQUIRED_BOT_PERMS.to_vec();
    if wants_text {
        need.extend(["SendMessages", "EmbedLinks"]);
    }
    let target = match target_parent {
        Some(p) => PermTarget::Channel(guild_id, p.id),
        None => PermTarget::Guild(guild_id),
    };
    let missing = missing_bot_permissions(app, target, &need);
    if missing.is_empty() {
        return None;
    }
    let where_ = match target_parent {
        Some(p) => format!("**{}**", shown(Some(&p.name))),
        None => "this server".into(),
    };
    Some(error_embed("Missing Permissions", Some(&format!("I need **{}** in {where_} to create temporary channels.", permission_names(&missing).join(", "))), None))
}

fn hub_channel_permission_error(app: &App, guild_id: GuildId, hub_channel: ChannelId) -> Option<CreateEmbed> {
    let missing = missing_bot_permissions(app, PermTarget::Channel(guild_id, hub_channel), &["ViewChannel", "Connect", "MoveMembers"]);
    if missing.is_empty() {
        return None;
    }
    Some(error_embed(
        "Missing Permissions",
        Some(&format!("I need **{}** in {} to move members out of the hub.", permission_names(&missing).join(", "), mention(hub_channel))),
        None,
    ))
}

fn bitrate_warning(app: &App, guild_id: GuildId, hub: &TempVoiceHub) -> Option<String> {
    let max = tv::with_guild(app, guild_id, tv::maximum_bitrate)?;
    match hub.bitrate {
        Some(b) if b != 0 && i64::from(b) > max => Some(format!(
            "This server currently allows up to {} kbps; new channels use that until the boost level rises.",
            bot_util::js::number_to_string(max as f64 / 1000.0)
        )),
        _ => None,
    }
}

fn warning_lines(warnings: &[String]) -> String {
    warnings.iter().map(|w| format!("\n⚠️ {w}")).collect()
}

async fn out(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
    Ok(())
}

fn picked_channel(ctx: &CommandCtx, name: &str) -> Option<(ChannelId, Option<GuildChannel>)> {
    let id = ctx.get_channel_id(name)?;
    let cached = ctx.guild_id().and_then(|g| cached_channel(&ctx.app, g, id));
    Some((id, cached))
}

/// [`picked_channel`] after the reply is deferred: a channel the cache lacks is fetched (and cached), as the JS
/// reads the option's resolved channel (`options.getChannel`), its `parent` included.
async fn picked_channel_fetched(ctx: &CommandCtx, name: &str) -> Option<(ChannelId, Option<GuildChannel>)> {
    let (id, cached) = picked_channel(ctx, name)?;
    if cached.is_some() {
        return Some((id, cached));
    }
    let fetched = match ctx.guild_id() {
        Some(g) => bot_core::interaction_helpers::resolve_channel(&ctx.app, g, id).await,
        None => None,
    };
    Some((id, fetched))
}

fn parent_of(app: &App, guild_id: GuildId, channel: Option<&GuildChannel>) -> Option<GuildChannel> {
    channel.and_then(|c| c.parent_id).and_then(|p| cached_channel(app, guild_id, p))
}

// ── create ────────────────────────────────────────────────────────────────────

async fn run_create(ctx: &CommandCtx, settings: Option<&bot_db::GuildSettings>) -> BotResult {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice-hub outside a guild"))?;
    let g = guild_id.to_string();
    let channel = picked_channel_fetched(ctx, "channel").await;
    let category = picked_channel_fetched(ctx, "category").await;
    let name_format = ctx.get_string("name_format");
    let limit = ctx.get_integer("limit");
    let bitrate = ctx.get_integer("bitrate");
    let keep_alive = ctx.get_integer("keep_alive");
    let ownership_lock = ctx.get_integer("ownership_lock");
    let text_channel = ctx.get_boolean("text_channel");
    let text_private = ctx.get_boolean("text_private");
    let sync_permissions = ctx.get_boolean("sync_permissions");

    if tv::count_hubs(app.db(), &g).await? >= limits::TEMPVOICE_HUBS as i64 {
        return out(ctx, limit_reached()).await;
    }
    if let Some((id, _)) = &channel {
        if tv::find_hub_by_channel(app.db(), &g, &id.to_string()).await?.is_some() {
            return out(ctx, already_a_hub(&mention(*id))).await;
        }
        if tv::is_temp_channel(app.db(), &id.to_string()).await? {
            return out(ctx, error_embed("Temporary Channel", Some(&format!("{} is a temporary channel and cannot be a hub.", mention(*id))), None)).await;
        }
    }
    let (preview, nf_warnings) = match check_name_format(ctx, name_format.as_deref()) {
        Ok(v) => v,
        Err(e) => return out(ctx, *e).await,
    };

    let target_parent = category.as_ref().and_then(|(_, c)| c.clone()).or_else(|| parent_of(app, guild_id, channel.as_ref().and_then(|(_, c)| c.as_ref())));
    let problem = permission_error(app, guild_id, target_parent.as_ref(), text_channel == Some(true))
        .or_else(|| channel.as_ref().and_then(|(id, _)| hub_channel_permission_error(app, guild_id, *id)));
    if let Some(p) = problem {
        return out(ctx, p).await;
    }

    let mut created: Option<ChannelId> = None;
    let hub_channel = match &channel {
        Some((id, _)) => *id,
        None => {
            let spec = NewChannel {
                name: tv::HUB_CHANNEL_NAME.into(),
                kind: ChannelType::Voice,
                parent: category.as_ref().map(|(id, _)| *id),
                user_limit: None,
                bitrate: None,
                topic: None,
                overwrites: Vec::new(),
                reason: Some(clamp_text(&format!("Temp voice hub created by {}", bot_core::adapters::user_tag(ctx.user())), 512)),
            };
            match tv::discord_call(app, "create hub channel", Some(&g), rest(app).create_channel(guild_id, spec)).await {
                Ok(c) => {
                    tv::cache_created_channel(app, &c);
                    created = Some(c.id);
                    c.id
                }
                Err(e) => {
                    return out(
                        ctx,
                        error_embed("Could Not Create Channel", Some(&format!("Discord refused to create the hub channel ({}).", tv::describe_discord_error(e.code))), None),
                    )
                    .await;
                }
            }
        }
    };
    let rollback = || async {
        if let Some(c) = created {
            let _ = tv::delete_discord_channel(app, guild_id, &c.to_string(), "Temp voice: hub was not saved", "rollback hub channel").await;
        }
    };

    // Only the options that were given: the schema defaults apply to the rest.
    let mut data: HubData = vec![("channelId", json!(hub_channel.to_string()))];
    if let Some((id, _)) = &category {
        data.push(("categoryId", json!(id.to_string())));
    }
    if let Some(nf) = &name_format {
        data.push(("nameFormat", json!(bot_util::js::trim(nf))));
    }
    if let Some(v) = limit {
        data.push(("userLimit", json!(v)));
    }
    if let Some(v) = bitrate {
        data.push(("bitrate", json!(v * 1000)));
    }
    if let Some(v) = keep_alive {
        data.push(("keepAliveMinutes", json!(v)));
    }
    if let Some(v) = ownership_lock {
        data.push(("ownershipLockMinutes", json!(v)));
    }
    if let Some(v) = text_channel {
        data.push(("createTextChannel", json!(v)));
    }
    if let Some(v) = text_private {
        data.push(("textChannelMembersOnly", json!(v)));
    }
    if let Some(v) = sync_permissions {
        data.push(("syncPermissions", json!(v)));
    }

    let hub = match tv::create_hub(app, &g, data).await {
        Ok(h) => h,
        Err(e) => {
            rollback().await;
            if e.downcast_ref::<tv::HubLimitError>().is_some() {
                return out(ctx, limit_reached()).await;
            }
            if tv::is_duplicate_hub(&e) {
                return out(ctx, already_a_hub(&mention(hub_channel))).await;
            }
            return Err(e);
        }
    };

    let mut warnings = nf_warnings;
    if let Some(bw) = bitrate_warning(app, guild_id, &hub) {
        warnings.push(bw);
    }
    let guild = cached_guild(app, guild_id);
    let text = format!(
        "Join {} to get your own voice channel.\n\n{}\n\n**Preview:** {}{}{}",
        mention(hub_channel),
        tv::hub_summary(&hub, guild.as_ref()),
        escape_markdown(&preview),
        warning_lines(&warnings),
        module_note(settings)
    );
    out(ctx, success_embed("Hub Created", Some(&text), None)).await
}

// ── edit ──────────────────────────────────────────────────────────────────────

async fn run_edit(ctx: &CommandCtx, settings: Option<&bot_db::GuildSettings>) -> BotResult {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice-hub outside a guild"))?;
    let g = guild_id.to_string();
    let (hub_id, hub_cached) = picked_channel_fetched(ctx, "hub").await.ok_or_else(|| anyhow::anyhow!("Required option \"hub\" not found."))?;
    let Some(hub) = tv::find_hub_by_channel(app.db(), &g, &hub_id.to_string()).await? else {
        return out(ctx, not_a_hub(&mention(hub_id))).await;
    };
    let category = picked_channel_fetched(ctx, "category").await;
    let name_format = ctx.get_string("name_format");
    let limit = ctx.get_integer("limit");
    let bitrate = ctx.get_integer("bitrate");
    let keep_alive = ctx.get_integer("keep_alive");
    let ownership_lock = ctx.get_integer("ownership_lock");
    let text_channel = ctx.get_boolean("text_channel");
    let text_private = ctx.get_boolean("text_private");
    let sync_permissions = ctx.get_boolean("sync_permissions");

    let any_given = category.is_some()
        || name_format.is_some()
        || limit.is_some()
        || bitrate.is_some()
        || keep_alive.is_some()
        || ownership_lock.is_some()
        || text_channel.is_some()
        || text_private.is_some()
        || sync_permissions.is_some();
    if !any_given {
        return out(ctx, error_embed("Nothing To Change", Some("Pass at least one setting to change."), None)).await;
    }
    let (_, nf_warnings) = match check_name_format(ctx, name_format.as_deref()) {
        Ok(v) => v,
        Err(e) => return out(ctx, *e).await,
    };

    let effective_parent = category
        .as_ref()
        .and_then(|(_, c)| c.clone())
        .or_else(|| hub.category_id.as_deref().and_then(|c| c.parse::<u64>().ok()).filter(|v| *v != 0).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c))))
        .or_else(|| parent_of(app, guild_id, hub_cached.as_ref()));
    if let Some(p) = permission_error(app, guild_id, effective_parent.as_ref(), text_channel.unwrap_or(hub.create_text_channel)) {
        return out(ctx, p).await;
    }

    let mut patch: HubData = Vec::new();
    if let Some((id, _)) = &category {
        patch.push(("categoryId", json!(id.to_string())));
    }
    if let Some(nf) = &name_format {
        patch.push(("nameFormat", json!(bot_util::js::trim(nf))));
    }
    if let Some(v) = limit {
        patch.push(("userLimit", json!(v)));
    }
    if let Some(v) = bitrate {
        patch.push(("bitrate", json!(v * 1000)));
    }
    if let Some(v) = keep_alive {
        patch.push(("keepAliveMinutes", json!(v)));
    }
    if let Some(v) = ownership_lock {
        patch.push(("ownershipLockMinutes", json!(v)));
    }
    if let Some(v) = text_channel {
        patch.push(("createTextChannel", json!(v)));
    }
    if let Some(v) = text_private {
        patch.push(("textChannelMembersOnly", json!(v)));
    }
    if let Some(v) = sync_permissions {
        patch.push(("syncPermissions", json!(v)));
    }
    let Some(updated) = tv::update_hub(app, &g, &hub.id, patch).await? else {
        return out(ctx, not_a_hub(&mention(hub_id))).await;
    };

    let mut lines: Vec<String> = Vec::new();
    if updated.user_limit != hub.user_limit {
        lines.push(format!("• **Limit:** {} → {}", limit_text(hub.user_limit), limit_text(updated.user_limit)));
    }
    if updated.keep_alive_minutes != hub.keep_alive_minutes {
        lines.push(format!("• **Keep alive:** {} → {}", keep_text(hub.keep_alive_minutes), keep_text(updated.keep_alive_minutes)));
    }
    if updated.name_format != hub.name_format {
        lines.push(format!("• **Name format:** `{}` → `{}`", hub.name_format.replace('`', "ˋ"), updated.name_format.replace('`', "ˋ")));
    }
    if updated.category_id != hub.category_id {
        lines.push(format!("• **Category:** {} → {}", category_text(hub.category_id.as_deref()), category_text(updated.category_id.as_deref())));
    }
    if updated.bitrate != hub.bitrate {
        lines.push(format!("• **Bitrate:** {} → {}", bitrate_text(hub.bitrate), bitrate_text(updated.bitrate)));
    }
    if updated.ownership_lock_minutes != hub.ownership_lock_minutes {
        lines.push(format!("• **Ownership lock:** {} → {}", lock_text(hub.ownership_lock_minutes), lock_text(updated.ownership_lock_minutes)));
    }
    if text_mode(&updated) != text_mode(&hub) {
        lines.push(format!("• **Text channel:** {} → {}", text_mode(&hub), text_mode(&updated)));
    }
    if updated.sync_permissions != hub.sync_permissions {
        lines.push(format!("• **Permissions:** {} → {}", perms_text(hub.sync_permissions), perms_text(updated.sync_permissions)));
    }
    if lines.is_empty() {
        return out(ctx, warn_embed("No Change", Some("The hub already has those settings."), None)).await;
    }
    let mut warnings = nf_warnings;
    if let Some(bw) = bitrate_warning(app, guild_id, &updated) {
        warnings.push(bw);
    }
    let text = format!(
        "{}\n\nName, limit, bitrate, permissions and text-channel settings apply to channels created from now on. Keep alive, ownership lock and roles apply to existing channels too.{}{}",
        lines.join("\n"),
        warning_lines(&warnings),
        module_note(settings)
    );
    out(ctx, success_embed("Hub Updated", Some(&text), None)).await
}

// ── delete ────────────────────────────────────────────────────────────────────

async fn run_delete(ctx: &CommandCtx) -> BotResult {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice-hub outside a guild"))?;
    let g = guild_id.to_string();
    let (hub_id, hub_cached) = picked_channel(ctx, "hub").ok_or_else(|| anyhow::anyhow!("Required option \"hub\" not found."))?;
    let delete_channel = ctx.get_boolean("delete_channel") == Some(true);
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(hub) = tv::find_hub_by_channel(app.db(), &g, &hub_id.to_string()).await? else {
        let _ = ctx.edit_reply(serenity::builder::EditInteractionResponse::new().embeds(vec![not_a_hub(&mention(hub_id))])).await;
        return Ok(());
    };
    let rows = tv::list_hub_channels(app.db(), &hub.id).await?;
    let occupied = rows
        .iter()
        .filter(|r| r.channel_id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|c| tv::count_humans(app, guild_id, ChannelId::new(c)) > 0))
        .count();
    let description = format!(
        "Delete the hub {}? Its **{}** temporary channel(s){} will be deleted too.{}",
        mention(hub_id),
        rows.len(),
        if occupied > 0 { format!(" (**{occupied}** with members in them)") } else { String::new() },
        if delete_channel { " The hub channel itself will also be deleted." } else { "" }
    );
    let confirmed = await_confirm(
        ctx,
        ConfirmOptions { title: Some("Delete hub?".into()), description: Some(description), confirm_label: "Delete".into(), ..Default::default() },
    )
    .await;
    if !confirmed {
        return Ok(()); // the prompt already shows the outcome
    }
    let tag = bot_core::adapters::user_tag(ctx.user());
    let r = tv::delete_hub(app, guild_id, &hub, delete_channel, Some(&tag)).await?;
    if r.not_found {
        let _ = ctx
            .follow_up(serenity::builder::CreateInteractionResponseFollowup::new().embed(error_embed("Not A Hub", Some("That hub was already deleted."), None)).ephemeral(true))
            .await;
        return Ok(());
    }
    let name = hub_cached.map(|c| c.name).or_else(|| ctx.get_channel("hub").and_then(|c| c.name));
    let text = format!(
        "The hub **{}** was deleted and **{}** temporary channel(s) were removed.{}{}",
        shown(name.as_deref()),
        r.deleted,
        if r.failed > 0 { format!("\n⚠️ {} channel(s) could not be deleted; delete them by hand.", r.failed) } else { String::new() },
        if r.hub_channel_failed { "\n⚠️ I could not delete the hub channel." } else { "" }
    );
    let _ = ctx.follow_up(serenity::builder::CreateInteractionResponseFollowup::new().embed(success_embed("Hub Deleted", Some(&text), None))).await;
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

async fn run_list(ctx: &CommandCtx, settings: Option<&bot_db::GuildSettings>) -> BotResult {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice-hub outside a guild"))?;
    let hubs = tv::list_hubs(app.db(), &guild_id.to_string()).await?;
    if hubs.is_empty() {
        return out(ctx, apply_guild_color(info_embed("Temporary Channel Hubs", Some(&format!("No hubs yet. Create one with {CREATE_HINT}.")), None), settings)).await;
    }
    let mut embed = apply_guild_color(info_embed("Temporary Channel Hubs", Some(&format!("{}/{} hubs", hubs.len(), limits::TEMPVOICE_HUBS)), None), settings);
    // A message's embeds may hold 6000 characters in all: share them out.
    let per_field = (5400 / hubs.len() as i64 - 60).clamp(0, 1024) as usize;
    let guild = cached_guild(app, guild_id);
    for h in hubs.iter().take(limits::TEMPVOICE_HUBS) {
        let hub_channel = h.hub.channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c)));
        let name = match &hub_channel {
            Some(c) => format!("🔊 #{}", c.name),
            None => "🔊 Deleted channel (retired)".into(),
        };
        let mut lines = vec![tv::hub_summary(&h.hub, guild.as_ref()), format!("Active channels: {}", h.channels)];
        lines.extend(tv::hub_problems(app, guild_id, &h.hub).into_iter().map(|p| format!("⚠️ {p}")));
        embed = embed.field(clamp_text(&name, 256), clamp_text(&lines.join("\n"), per_field), false);
    }
    out(ctx, embed).await
}

// ── ignore-role / mod-role ────────────────────────────────────────────────────

async fn run_role_list(ctx: &CommandCtx, group: &str, sub: &str) -> BotResult {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice-hub outside a guild"))?;
    let g = guild_id.to_string();
    let (hub_id, _) = picked_channel(ctx, "hub").ok_or_else(|| anyhow::anyhow!("Required option \"hub\" not found."))?;
    let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let Some(hub) = tv::find_hub_by_channel(app.db(), &g, &hub_id.to_string()).await? else {
        return out(ctx, not_a_hub(&mention(hub_id))).await;
    };
    // These roles are only read for authorization and overwrites, never assigned.
    if role.get() == guild_id.get() {
        return out(ctx, error_embed("Invalid Role", Some("@everyone cannot be used here."), None)).await;
    }
    let (field, label, plural) =
        if group == "ignore-role" { ("ignoredRoleIds", "an ignored role", "ignored roles") } else { ("moderatorRoleIds", "a moderator role", "moderator roles") };
    let role_m = format!("<@&{role}>");
    match tv::update_hub_role_list(app, &g, &hub.id, field, sub, &role.to_string()).await? {
        RoleListOutcome::Gone => return out(ctx, not_a_hub(&mention(hub_id))).await,
        RoleListOutcome::Exists => return out(ctx, warn_embed("No Change", Some(&format!("{role_m} is already {label}.")), None)).await,
        RoleListOutcome::Missing => return out(ctx, warn_embed("No Change", Some(&format!("{role_m} is not {label}.")), None)).await,
        RoleListOutcome::Limit => {
            return out(ctx, error_embed("Limit Reached", Some(&format!("A hub can have at most {} {plural}.", tv::MAX_HUB_ROLE_LIST)), None)).await;
        }
        RoleListOutcome::Ok => {}
    }
    let settings = app.settings().get_settings_safe(&g).await;
    let note = module_note(settings.as_deref());
    let hub_m = mention(hub_id);
    let embed = match (group, sub) {
        ("ignore-role", "add") => {
            success_embed("Ignored Role Added", Some(&format!("/voice commands no longer affect members with {role_m} in channels from {hub_m}.{note}")), None)
        }
        ("ignore-role", _) => success_embed("Ignored Role Removed", Some(&format!("/voice commands affect members with {role_m} again.{note}")), None),
        (_, "add") => success_embed("Moderator Role Added", Some(&format!("Members with {role_m} can use every /voice command on channels from {hub_m}.{note}")), None),
        _ => success_embed("Moderator Role Removed", Some(&format!("{role_m} is no longer a temp-voice moderator role.{note}")), None),
    };
    out(ctx, embed).await
}

// ── execute ───────────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let res: BotResult = async {
        if sub == "delete" && group.is_none() {
            return run_delete(&ctx).await;
        }
        if !safe_defer(&ctx.responder, false).await {
            return Ok(());
        }
        if !resolve_new_options(&mut ctx, NewOptions { required: &[], private: &[] }).await {
            return Ok(());
        }
        let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
        let settings = ctx.app.settings().get_settings_safe(&guild_id).await;
        if let Some(gr) = &group {
            run_role_list(&ctx, gr, &sub).await
        } else if sub == "create" {
            run_create(&ctx, settings.as_deref()).await
        } else if sub == "edit" {
            run_edit(&ctx, settings.as_deref()).await
        } else {
            run_list(&ctx, settings.as_deref()).await
        }
    }
    .await;
    if let Err(e) = res {
        let label = match &group {
            Some(gr) => format!("{gr} {sub}"),
            None => sub.clone(),
        };
        bot_util::log_error!(format!("voice-hub {label} failed"), bot_core::error::log_arg(&e));
        safe_reply(&ctx.responder, Payload::embed(internal_error()).ephemeral(true)).await;
    }
    Ok(())
}
