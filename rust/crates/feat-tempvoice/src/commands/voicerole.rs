//! Port of `src/commands/admin/voicerole.js`: `/voicerole add | remove | any add | any remove | list | sync`. An admin
//! config command: replies are public and it keeps working while the Voice Roles module is off (`gated = false`);
//! only `sync` refuses while the module is off. Hooks: [`crate::plugins::voice_roles`]; rules:
//! [`crate::voice_role_service`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{PermTarget, Payload, check_assignable_role, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_core::settings::is_module_enabled;
use bot_config::limits;
use bot_util::text::{chunk_lines, format_number, join_with_limit};
use serenity::all::{ChannelType, Guild, GuildId, Permissions, Role};
use serenity::builder::CreateEmbed;

use crate::voice_role_service::{
    AddOutcome, RECONCILE_MAX, add_any_role, add_link, any_roles_of, has_voice_roles, links_of, reconcile_guild, remove_any_role, remove_link,
};

const LINKABLE_TYPES: [u8; 3] = [channel_type::GUILD_VOICE, channel_type::GUILD_STAGE_VOICE, channel_type::GUILD_CATEGORY];

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("voicerole", "Give members a role while they are in a voice channel.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("add", "Give a role to members in a voice channel or category.")
                .option(Opt::channel("channel", "A voice channel, stage or category.").channel_types(&LINKABLE_TYPES).required(true))
                .option(Opt::role("role", "A role made only for this.").required(true)),
        )
        .option(
            Opt::sub_command("remove", "Stop giving a role to a voice channel or category.")
                .option(Opt::channel("channel", "The channel or category.").channel_types(&LINKABLE_TYPES).required(true))
                .option(Opt::role("role", "The role.").required(true)),
        )
        .option(
            Opt::sub_command_group("any", "Roles for every voice channel.")
                .option(Opt::sub_command("add", "Give a role to members in any voice channel.").option(Opt::role("role", "A role made only for this.").required(true)))
                .option(Opt::sub_command("remove", "Stop giving a role in every voice channel.").option(Opt::role("role", "The role.").required(true))),
        )
        .option(Opt::sub_command("list", "Show the voice roles and any problems."))
        .option(Opt::sub_command("sync", "Fix the roles of everyone now."))
}

/// The command (`module 'voiceroles'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Voiceroles), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}
fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}
fn everyone() -> CreateEmbed {
    error_embed("Invalid Role", Some("@everyone cannot be a voice role."), None)
}
fn bad_channel() -> CreateEmbed {
    error_embed("Invalid Channel", Some("Pick a voice channel, a stage channel or a category."), None)
}
fn links_full() -> CreateEmbed {
    error_embed(
        "List Full",
        Some(&format!("At most {} channel roles can be set up. Remove one with `/voicerole remove` first.", limits::VOICE_ROLE_LINKS)),
        None,
    )
}
fn any_full() -> CreateEmbed {
    error_embed(
        "List Full",
        Some(&format!("At most {} roles can be given in every voice channel. Remove one with `/voicerole any remove` first.", limits::VOICE_ROLE_ANY)),
        None,
    )
}

pub const OFF_NOTE: &str = "\n\n⚠️ The **Voice Roles** module is off. Turn it on with `/module enable voiceroles`.";
pub const TAKES_ROLE_WARNING: &str =
    "The bot **takes this role away from anyone who is not in the linked channel**, so it must be a role made only for this.";
pub const WHEN_NOTE: &str = "It applies from each member's next voice join or move, or when you run `/voicerole sync`.";

/// `finish(interaction, embed)`: the final answer (overwrites the deferred reply); never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn gid(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voicerole outside a guild"))
}

fn guild_of(ctx: &CommandCtx) -> Option<Guild> {
    ctx.guild_id().and_then(|g| bot_core::interaction_helpers::cached_guild(&ctx.app, g))
}

/// `roleRefusal(interaction, role)`: an error embed when the role cannot be a voice role, otherwise `None`.
fn role_refusal(ctx: &CommandCtx, role: &Role) -> Option<CreateEmbed> {
    let guild_id = ctx.guild_id()?;
    if role.id.get() == guild_id.get() {
        return Some(everyone());
    }
    let guild = guild_of(ctx);
    let bot = ctx.app.discord().map(|d| d.cache.current_user().id).unwrap_or_default();
    let chk = match &guild {
        Some(g) => check_assignable_role(g, Some(role), bot, ctx.member(), false),
        None => bot_core::interaction_helpers::RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
    };
    if chk.ok { None } else { Some(error_embed("Cannot Use That Role", Some(&chk.reason.unwrap_or_default()), None)) }
}

/// The module-off note for a reply, read after the change was saved.
async fn off_note(ctx: &CommandCtx, guild_id: &str) -> &'static str {
    match ctx.app.settings().get_settings_safe(guild_id).await {
        Some(s) if !is_module_enabled(Some(&s), Some(ModuleKey::Voiceroles)) => OFF_NOTE,
        _ => "",
    }
}

/// `roleOf(interaction)`: the cached role, else the option's.
fn role_of(ctx: &CommandCtx) -> anyhow::Result<Role> {
    let id = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    if let Some(r) = guild_of(ctx).and_then(|g| g.roles.get(&id).cloned()) {
        return Ok(r);
    }
    ctx.get_role("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))
}

/// `channelOf(interaction)`: the channel id and type (cached first).
fn channel_of(ctx: &CommandCtx) -> anyhow::Result<(String, u8)> {
    let id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let guild_id = gid(ctx)?;
    if let Some(c) = bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, id) {
        return Ok((c.id.to_string(), u8::from(c.kind)));
    }
    let kind = ctx.get_channel("channel").map(|c| u8::from(c.kind)).unwrap_or(0);
    Ok((id.to_string(), kind))
}

fn where_text(channel_id: &str, kind: u8) -> String {
    if kind == u8::from(ChannelType::Category) {
        format!("any voice channel in the <#{channel_id}> category")
    } else {
        format!("<#{channel_id}>")
    }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    if group.as_deref() == Some("any") {
        return if sub == "add" { run_any_add(ctx).await } else { run_any_remove(ctx).await };
    }
    match sub.as_str() {
        "add" => run_add(ctx).await,
        "remove" => run_remove(ctx).await,
        "list" => run_list(ctx).await,
        "sync" => run_sync(ctx).await,
        _ => Ok(()),
    }
}

// ── /voicerole add | remove ───────────────────────────────────────────────────

async fn run_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?.to_string();
    let (channel_id, kind) = channel_of(&ctx)?;
    if !LINKABLE_TYPES.contains(&kind) {
        return finish(&ctx, bad_channel()).await;
    }
    let role = role_of(&ctx)?;
    if let Some(refusal) = role_refusal(&ctx, &role) {
        return finish(&ctx, refusal).await;
    }
    let outcome = match add_link(&ctx.app, &guild_id, &channel_id, &role.id.to_string()).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[voice-roles] {guild_id}: add failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    match outcome {
        AddOutcome::Full => return finish(&ctx, links_full()).await,
        AddOutcome::Same => {
            return finish(&ctx, info_embed("No Change", Some(&format!("{} already gives <@&{}>.", where_text(&channel_id, kind), role.id)), None)).await;
        }
        AddOutcome::Changed => {}
    }
    bot_util::log_info!(format!("[voice-roles] {guild_id}: link {channel_id} -> {} by {}", role.id, ctx.user().id));
    let note = off_note(&ctx, &guild_id).await;
    let text = format!(
        "Members in {} get <@&{}> while they are there.\n\n⚠️ {TAKES_ROLE_WARNING}\n{WHEN_NOTE}{note}",
        where_text(&channel_id, kind),
        role.id
    );
    finish(&ctx, success_embed("Voice Role Added", Some(&text), None)).await
}

fn removed_text(role_id: &str, still_managed: bool) -> String {
    if still_managed {
        format!("<@&{role_id}> is still given somewhere else, so the bot keeps managing it.")
    } else {
        format!("The bot no longer manages <@&{role_id}>: members who have it keep it until you take it from them.")
    }
}

async fn run_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?.to_string();
    let (channel_id, kind) = channel_of(&ctx)?;
    if !LINKABLE_TYPES.contains(&kind) {
        return finish(&ctx, bad_channel()).await;
    }
    let role = role_of(&ctx)?;
    let rid = role.id.to_string();
    let res = match remove_link(&ctx.app, &guild_id, &channel_id, &rid).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[voice-roles] {guild_id}: remove failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    if !res.changed {
        return finish(&ctx, info_embed("No Change", Some(&format!("{} does not give <@&{rid}>.", where_text(&channel_id, kind))), None)).await;
    }
    bot_util::log_info!(format!("[voice-roles] {guild_id}: unlink {channel_id} -> {rid} by {}", ctx.user().id));
    let note = off_note(&ctx, &guild_id).await;
    let text = format!("{} no longer gives <@&{rid}>.\n{}{note}", where_text(&channel_id, kind), removed_text(&rid, res.still_managed));
    finish(&ctx, success_embed("Voice Role Removed", Some(&text), None)).await
}

// ── /voicerole any add | remove ───────────────────────────────────────────────

async fn run_any_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?.to_string();
    let role = role_of(&ctx)?;
    if let Some(refusal) = role_refusal(&ctx, &role) {
        return finish(&ctx, refusal).await;
    }
    let rid = role.id.to_string();
    let outcome = match add_any_role(&ctx.app, &guild_id, &rid).await {
        Ok(o) => o,
        Err(e) => {
            bot_util::log_error!(format!("[voice-roles] {guild_id}: any add failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    match outcome {
        AddOutcome::Full => return finish(&ctx, any_full()).await,
        AddOutcome::Same => return finish(&ctx, info_embed("No Change", Some(&format!("<@&{rid}> is already given in every voice channel.")), None)).await,
        AddOutcome::Changed => {}
    }
    bot_util::log_info!(format!("[voice-roles] {guild_id}: any-role {rid} added by {}", ctx.user().id));
    let note = off_note(&ctx, &guild_id).await;
    let text = format!(
        "Members in any voice channel get <@&{rid}> while they are there.\n\n⚠️ The bot **takes this role away from anyone who is not in a voice channel**, so it must be a role made only for this.\n{WHEN_NOTE}{note}"
    );
    finish(&ctx, success_embed("Voice Role Added", Some(&text), None)).await
}

async fn run_any_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?.to_string();
    let role = role_of(&ctx)?;
    let rid = role.id.to_string();
    let res = match remove_any_role(&ctx.app, &guild_id, &rid).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[voice-roles] {guild_id}: any remove failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    if !res.changed {
        return finish(&ctx, info_embed("No Change", Some(&format!("<@&{rid}> is not given in every voice channel.")), None)).await;
    }
    bot_util::log_info!(format!("[voice-roles] {guild_id}: any-role {rid} removed by {}", ctx.user().id));
    let note = off_note(&ctx, &guild_id).await;
    let text = format!("<@&{rid}> is no longer given in every voice channel.\n{}{note}", removed_text(&rid, res.still_managed));
    finish(&ctx, success_embed("Voice Role Removed", Some(&text), None)).await
}

// ── /voicerole list ───────────────────────────────────────────────────────────

/// `roleProblem(guild, roleId)`: why the bot will not use a role (deleted, above the bot, managed, dangerous).
pub fn role_problem(app: &App, guild: Option<&Guild>, role_id: &str) -> Option<String> {
    let rid = role_id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::RoleId::new);
    let role = guild.zip(rid).and_then(|(g, r)| g.roles.get(&r));
    let (Some(guild), Some(role)) = (guild, role) else { return Some("the role was deleted".into()) };
    let bot = app.discord().map(|d| d.cache.current_user().id).unwrap_or_default();
    let chk = check_assignable_role(guild, Some(role), bot, None, false);
    if chk.ok { None } else { chk.reason }
}

async fn run_list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let guild = guild_of(&ctx);
    let enabled = is_module_enabled(Some(&settings), Some(ModuleKey::Voiceroles));
    let links = links_of(Some(&settings));
    let any = any_roles_of(Some(&settings));

    let link_lines: Vec<String> = links
        .iter()
        .map(|l| {
            let channel = guild.as_ref().and_then(|g| l.channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| g.channels.get(&serenity::all::ChannelId::new(c))));
            let problem = match channel {
                None => Some("the channel was deleted".to_string()),
                Some(_) => role_problem(&ctx.app, guild.as_ref(), &l.role_id),
            };
            let where_ = if channel.is_some_and(|c| c.kind == ChannelType::Category) { format!("📁 <#{}>", l.channel_id) } else { format!("<#{}>", l.channel_id) };
            let mark = if problem.is_some() { "⚠️" } else { "✅" };
            let tail = problem.map(|p| format!(" — {p}")).unwrap_or_default();
            format!("{mark} {where_} → <@&{}>{tail}", l.role_id)
        })
        .collect();
    let any_lines: Vec<String> = any
        .iter()
        .map(|id| {
            let problem = role_problem(&ctx.app, guild.as_ref(), id);
            let mark = if problem.is_some() { "⚠️" } else { "✅" };
            format!("{mark} <@&{id}>{}", problem.map(|p| format!(" — {p}")).unwrap_or_default())
        })
        .collect();

    let mut embed = guild_embed(Some(&settings), Some("🎙️ Voice Roles"), None, None)
        .field("Status", if enabled { "✅ On" } else { "❌ Off — `/module enable voiceroles`" }, true)
        .field("AFK channel", if settings.voice_role_ignore_afk { "Gets no roles" } else { "Gets the roles too" }, true);
    let chunks: Vec<String> = chunk_lines(&link_lines, 1024).into_iter().take(3).collect();
    if chunks.is_empty() {
        embed = embed.field(format!("Channel roles (0/{})", limits::VOICE_ROLE_LINKS), "None — `/voicerole add`", false);
    } else {
        for (i, value) in chunks.into_iter().enumerate() {
            let name = if i == 0 { format!("Channel roles ({}/{})", links.len(), limits::VOICE_ROLE_LINKS) } else { "Channel roles (continued)".into() };
            embed = embed.field(name, value, false);
        }
    }
    let any_value = if any_lines.is_empty() { "None — `/voicerole any add`".to_string() } else { join_with_limit(&any_lines, "\n", 1024, bot_util::text::default_more) };
    embed = embed.field(format!("Any voice channel ({}/{})", any.len(), limits::VOICE_ROLE_ANY), any_value, false);
    let missing = if has_voice_roles(Some(&settings)) { missing_bot_permissions(&ctx.app, PermTarget::Guild(guild_id), &["ManageRoles"]) } else { Vec::new() };
    if !missing.is_empty() {
        embed = embed.field("Problem", format!("⚠️ I need **{}**.", permission_names(&missing).join(", ")), false);
    }
    embed = embed.footer(serenity::builder::CreateEmbedFooter::new(
        "The bot takes these roles from anyone not in the linked channel. Roles with powerful permissions, managed roles and roles above mine are never given.",
    ));
    finish(&ctx, embed).await
}

// ── /voicerole sync ───────────────────────────────────────────────────────────

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

async fn run_sync(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = gid(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Voiceroles)) {
        return finish(
            &ctx,
            warn_embed(
                "Module Is Off",
                Some("Voice roles are only kept up to date while the **Voice Roles** module is on. Turn it on with `/module enable voiceroles`, then run this again."),
                None,
            ),
        )
        .await;
    }
    if !has_voice_roles(Some(&settings)) {
        return finish(&ctx, info_embed("Nothing To Sync", Some("No voice roles are set up yet. Use `/voicerole add` or `/voicerole any add`."), None)).await;
    }
    let missing = missing_bot_permissions(&ctx.app, PermTarget::Guild(guild_id), &["ManageRoles"]);
    if !missing.is_empty() {
        return finish(&ctx, error_embed("Missing Permissions", Some(&format!("I need **{}** to change roles.", permission_names(&missing).join(", "))), None)).await;
    }
    let res = reconcile_guild(&ctx.app, guild_id, Some(settings), RECONCILE_MAX).await;
    if res.busy {
        return finish(&ctx, info_embed("Already Running", Some("A sync is already running on this server. Try again in a moment."), None)).await;
    }
    bot_util::log_info!(format!(
        "[voice-roles] {g}: sync by {}: checked {}, +{} -{}, {} failed",
        ctx.user().id,
        res.checked,
        res.added,
        res.removed,
        res.failed
    ));
    let f = |n: usize| format_number(n as f64);
    let mut lines = vec![format!(
        "Checked **{}** member{}: gave **{}** role{} and took **{}** away.",
        f(res.checked),
        plural(res.checked),
        f(res.added),
        plural(res.added),
        f(res.removed)
    )];
    if res.failed > 0 {
        lines.push(format!(
            "{} member{} could not be updated. Check my role position and permissions with `/voicerole list`.",
            f(res.failed),
            plural(res.failed)
        ));
    }
    if res.truncated {
        lines.push(format!("Only the first {} members were checked. Run `/voicerole sync` again to continue.", f(RECONCILE_MAX)));
    }
    lines.push("Only members the bot has cached are checked, so a holder who is not cached is fixed on their next voice change.".into());
    let text = lines.join("\n");
    let embed = if res.failed > 0 { warn_embed("Voice Roles Synced", Some(&text), None) } else { success_embed("Voice Roles Synced", Some(&text), None) };
    finish(&ctx, embed).await
}
