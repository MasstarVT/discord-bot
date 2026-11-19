//! Port of `src/commands/admin/backup.js`: `/backup create | list | info | export | status | delete | restore`. Admin
//! command (ephemeral replies: a backup describes the whole server) that keeps working while the Server Backups module
//! is off (`gated = false`). DELETE and RESTORE are for the SERVER OWNER only (the guild's `owner_id`; the bot owner's
//! level does not count), checked first, before any await, and again in the confirmation button.

use std::future::Future;

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, cached_guild, respond_autocomplete, safe_defer, safe_reply};
use bot_core::prelude::*;
use serde_json::Value;
use serenity::all::{CreateAttachment, CreateEmbed, Permissions};

use crate::backup_service::{
    self as svc, BackupInfo, CreateResult, StatusInput, backup, backup_label, build_info_embed, build_list_embed, build_preview_embed, build_status_embed,
    count_backups, create_backup, delete_backup, ensure_auto_job, get_backup, get_backup_with_data, get_restore_job, list_backups, missing_permissions_embed,
    missing_restore_permissions, newest_automatic, owner_only_embed, restore_buttons, restore_preview, search_backups,
};
use crate::guild_snapshot::{RESTORE_MODES, Snapshot, SnapshotError, exportable, parse_snapshot};
use crate::guild_snapshot::RestoreApi as _;
use crate::live_guild::LiveGuild;

fn backup_option() -> Opt {
    Opt::string("backup", "Which backup (type a name).").required(true).autocomplete(true)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("backup", "Back up and restore roles, channels and permissions.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("create", "Save the roles, channels and permissions now.").option(Opt::string("name", "A name for it.").max_length(backup::NAME_MAX as u16)))
        .option(Opt::sub_command("list", "Show the saved backups."))
        .option(Opt::sub_command("info", "Show what a backup holds.").option(backup_option()))
        .option(Opt::sub_command("export", "Download a backup as a JSON file.").option(backup_option()))
        .option(Opt::sub_command("status", "Show the schedule and any restore in progress."))
        .option(Opt::sub_command("delete", "Delete a backup. Server owner only.").option(backup_option()))
        .option(
            Opt::sub_command("restore", "Rebuild roles and channels from a backup. Server owner only.").option(backup_option()).option(
                Opt::string("mode", "What to rebuild (default: missing only).")
                    .choice("Missing only: recreate what is gone", "missing")
                    .choice("Full: also reset what exists", "full"),
            ),
        )
}

/// `/backup` (`module 'backups'`, `gated = false`, ADMIN, autocomplete).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Backups), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

fn not_found() -> CreateEmbed {
    error_embed("Backup Not Found", Some("Pick a backup from the list that appears as you type in the **backup** option."), None)
}
fn failed(what: &str) -> CreateEmbed {
    error_embed(&format!("Could Not {what}"), Some("Something went wrong. Please try again in a moment."), None)
}
fn unreadable(err: Option<&SnapshotError>) -> CreateEmbed {
    error_embed("Backup Unreadable", Some(err.map(|e| e.0.as_str()).unwrap_or("The backup could not be read.")), None)
}

fn guild_id_of(ctx: &CommandCtx) -> String {
    ctx.guild_id().map(|g| g.to_string()).unwrap_or_default()
}

/// `interaction.user.id === interaction.guild.ownerId` (synchronous, from the cache).
fn is_owner(ctx: &CommandCtx) -> bool {
    ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g)).is_some_and(|g| g.owner_id == ctx.user().id)
}

/// Always private: a follow-up after a confirm dialog is a new message and would otherwise be public.
async fn finish(ctx: &CommandCtx, payload: Payload) {
    safe_reply(&ctx.responder, payload.no_mentions().ephemeral(true)).await;
}

async fn finish_embed(ctx: &CommandCtx, embed: CreateEmbed) {
    finish(ctx, Payload::embed(embed)).await;
}

/// A restore in progress for this backup (it must not be deleted under the job).
async fn restoring_this(ctx: &CommandCtx, guild_id: &str, backup_id: &str) -> bool {
    let job = get_restore_job(&ctx.app, guild_id).await;
    svc::is_live(job.as_ref()) && job.as_ref().and_then(|j| j.payload.get("backupId")).and_then(Value::as_str) == Some(backup_id)
}

/// Keeps the recurring backup.auto job alive whenever an admin touches the module (a dashboard change alone cannot).
fn nudge_schedule(ctx: &CommandCtx, guild_id: &str, settings: Option<&bot_db::GuildSettings>) {
    if let Some(s) = settings
        && is_module_enabled(Some(s), Some(ModuleKey::Backups))
        && s.backup_interval_hours > 0
    {
        let (app, g) = (ctx.app.clone(), guild_id.to_string());
        tokio::spawn(async move {
            ensure_auto_job(&app, &g, None).await;
        });
    }
}

// ── /backup create ────────────────────────────────────────────────────────────

async fn run_create(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_id_of(ctx);
    // `createBackup({ guild: interaction.guild })`: without the guild the JS throws ("Could Not Finish"); an empty
    // view would store an empty backup.
    let live = LiveGuild::new(&ctx.app, &guild_id);
    if live.cached().is_none() {
        anyhow::bail!("/backup create: the guild is not cached");
    }
    let view = live.view();
    let user = ctx.user().id.to_string();
    let name = ctx.get_string("name");
    let backup = match create_backup(&ctx.app, &view, Some(&user), name.as_deref(), false).await? {
        CreateResult::Refused { error, .. } => {
            finish_embed(ctx, error_embed("Could Not Make The Backup", Some(&error), None)).await;
            return Ok(());
        }
        CreateResult::Ok(b) => b,
    };
    let settings = ctx.app.settings().get_settings_safe(&guild_id).await;
    nudge_schedule(ctx, &guild_id, settings.as_deref());
    bot_util::log_info!(format!("[backups] {guild_id}: backup {} made by {user}", backup.id));
    finish_embed(
        ctx,
        success_embed(
            "Backup Saved",
            Some(&format!(
                "Saved **{}** roles and **{}** categories and channels with their permissions.\nThreads, messages, members and their roles, emojis, stickers, webhooks and bot settings are not part of a backup.",
                bot_util::text::format_number(backup.role_count as f64),
                bot_util::text::format_number(backup.channel_count as f64)
            )),
            Some(&format!("ID {}", backup.id)),
        ),
    )
    .await;
    Ok(())
}

// ── /backup list | info | export ──────────────────────────────────────────────

async fn run_list(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_id_of(ctx);
    let (settings, rows) = tokio::join!(ctx.app.settings().get_settings_safe(&guild_id), list_backups(ctx.app.db(), &guild_id));
    finish_embed(ctx, build_list_embed(settings.as_deref(), &rows?)).await;
    Ok(())
}

enum Loaded {
    Ok(Box<bot_db::ServerBackup>, Box<Snapshot>),
    Error(Box<CreateEmbed>),
}

async fn load_snapshot(ctx: &CommandCtx) -> anyhow::Result<Loaded> {
    let id = ctx.get_string("backup").ok_or_else(|| anyhow::anyhow!("Required option \"backup\" not found."))?;
    let Some(row) = get_backup_with_data(ctx.app.db(), &guild_id_of(ctx), &id).await? else { return Ok(Loaded::Error(Box::new(not_found()))) };
    Ok(match parse_snapshot(&row.data) {
        Ok(s) => Loaded::Ok(Box::new(row), Box::new(s)),
        Err(e) => Loaded::Error(Box::new(unreadable(Some(&e)))),
    })
}

async fn run_info(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let settings = ctx.app.settings().get_settings_safe(&guild_id_of(ctx)).await;
    match load_snapshot(ctx).await? {
        Loaded::Error(e) => finish_embed(ctx, *e).await,
        Loaded::Ok(row, snapshot) => finish_embed(ctx, build_info_embed(settings.as_deref(), &BackupInfo::of(&row), &snapshot)).await,
    }
    Ok(())
}

/// The export file: `backup-YYYYMMDD-<last 8 of id>.json`, pretty JSON without the id map.
pub fn export_file(row: &bot_db::ServerBackup) -> (String, Vec<u8>) {
    let day = row.created_at.format("%Y%m%d").to_string();
    let id: Vec<char> = row.id.chars().collect();
    let tail: String = id[id.len().saturating_sub(8)..].iter().collect();
    let body = serde_json::to_string_pretty(&exportable(&row.data)).unwrap_or_default();
    (format!("backup-{day}-{tail}.json"), body.into_bytes())
}

async fn run_export(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let row = match load_snapshot(ctx).await? {
        Loaded::Error(e) => {
            finish_embed(ctx, *e).await;
            return Ok(());
        }
        Loaded::Ok(row, _) => row,
    };
    let (name, bytes) = export_file(&row);
    let embed = success_embed(
        "Backup Exported",
        Some(&format!(
            "**{}**. Only you can see this file. It lists role permissions and channel layout, so keep it private.",
            backup_label(&BackupInfo::of(&row))
        )),
        None,
    );
    finish(ctx, Payload::embed(embed).file(CreateAttachment::bytes(bytes, name))).await;
    Ok(())
}

// ── /backup status ────────────────────────────────────────────────────────────

async fn run_status(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_id_of(ctx);
    let settings = ctx.app.settings().get_settings_safe(&guild_id).await;
    let (counts, newest, restore_job) = tokio::join!(count_backups(ctx.app.db(), &guild_id), newest_automatic(ctx.app.db(), &guild_id), get_restore_job(&ctx.app, &guild_id));
    let (counts, newest) = (counts?, newest?);
    let mut problems = vec![];
    let live = LiveGuild::new(&ctx.app, &guild_id);
    if live.cached().is_none() {
        anyhow::bail!("/backup status: the guild is not cached"); // `guild.members` of a null guild throws in the JS
    }
    let me = live.view().me;
    if !me.as_ref().is_some_and(|m| m.has(crate::guild_snapshot::perm::MANAGE_ROLES)) {
        problems.push("I need **Manage Roles** to restore roles and channel permissions.".to_string());
    }
    if !me.as_ref().is_some_and(|m| m.has(crate::guild_snapshot::perm::MANAGE_CHANNELS)) {
        problems.push("I need **Manage Channels** to restore categories and channels.".to_string());
    }
    if let Some(s) = settings.as_deref()
        && !is_module_enabled(Some(s), Some(ModuleKey::Backups))
    {
        problems.push("The module is off: automatic backups are paused. `/module enable backups` turns them on. Manual backups and restores still work.".to_string());
    }
    nudge_schedule(ctx, &guild_id, settings.as_deref());
    let embed = build_status_embed(StatusInput {
        settings: settings.as_deref(),
        counts,
        newest: newest.map(|(_, at)| at),
        restore_job: restore_job.as_ref(),
        problems,
        now_ms: ctx.app.clock().now_ms(),
    });
    finish_embed(ctx, embed).await;
    Ok(())
}

// ── /backup delete (server owner) ─────────────────────────────────────────────

/// `/backup delete` with the confirm prompt (`awaitConfirm`).
pub async fn run_delete(ctx: &CommandCtx) -> anyhow::Result<()> {
    run_delete_with(ctx, |ctx, opts| async move { await_confirm(ctx, opts).await }).await
}

/// `runDelete(interaction, { confirm })`: the test seam replaces the confirm prompt.
pub async fn run_delete_with<'a, F, Fut>(ctx: &'a CommandCtx, confirm: F) -> anyhow::Result<()>
where
    F: FnOnce(&'a CommandCtx, ConfirmOptions) -> Fut,
    Fut: Future<Output = bool> + 'a,
{
    if !is_owner(ctx) {
        safe_reply(&ctx.responder, Payload::embed(owner_only_embed()).ephemeral(true)).await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_id_of(ctx);
    let id = ctx.get_string("backup").ok_or_else(|| anyhow::anyhow!("Required option \"backup\" not found."))?;
    let Some(row) = get_backup(ctx.app.db(), &guild_id, &id).await? else {
        finish_embed(ctx, not_found()).await;
        return Ok(());
    };
    if restoring_this(ctx, &guild_id, &row.id).await {
        finish_embed(ctx, warn_embed("Restore Running", Some("This backup is being restored right now. Wait until the restore is finished."), None)).await;
        return Ok(());
    }
    let yes = confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete this backup?".into()),
            description: Some(format!(
                "**{}** ({} roles, {} channels) will be deleted for good.",
                backup_label(&row),
                bot_util::text::format_number(row.role_count as f64),
                bot_util::text::format_number(row.channel_count as f64)
            )),
            confirm_label: "Delete".into(),
            danger: true,
            timeout_ms: 30_000.0,
            ..Default::default()
        },
    )
    .await;
    if !yes {
        return Ok(());
    }
    // The restore may have started while the owner thought about it.
    if restoring_this(ctx, &guild_id, &row.id).await {
        finish_embed(ctx, warn_embed("Restore Running", Some("A restore of this backup just started, so it was not deleted."), None)).await;
        return Ok(());
    }
    let count = delete_backup(ctx.app.db(), &guild_id, &row.id).await?;
    bot_util::log_info!(format!("[backups] {guild_id}: backup {} deleted by {}", row.id, ctx.user().id));
    let embed = if count > 0 { success_embed("Backup Deleted", Some(&format!("**{}** is gone.", backup_label(&row))), None) } else { not_found() };
    finish_embed(ctx, embed).await;
    Ok(())
}

// ── /backup restore (server owner) ────────────────────────────────────────────

pub async fn run_restore(ctx: &CommandCtx) -> anyhow::Result<()> {
    if !is_owner(ctx) {
        safe_reply(&ctx.responder, Payload::embed(owner_only_embed()).ephemeral(true)).await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_id_of(ctx);
    let mode = ctx.get_string("mode").unwrap_or_else(|| "missing".into());
    if !RESTORE_MODES.contains(&mode.as_str()) {
        finish_embed(ctx, failed("Restore")).await;
        return Ok(());
    }
    let (row, snapshot) = match load_snapshot(ctx).await? {
        Loaded::Error(e) => {
            finish_embed(ctx, *e).await;
            return Ok(());
        }
        Loaded::Ok(r, s) => (r, s),
    };
    let job = get_restore_job(&ctx.app, &guild_id).await;
    if svc::is_live(job.as_ref()) {
        finish_embed(ctx, warn_embed("Restore Already Running", Some("A restore is already running on this server. Check `/backup status`."), None)).await;
        return Ok(());
    }
    let view = LiveGuild::new(&ctx.app, &guild_id).view();
    let missing = missing_restore_permissions(view.me.as_ref());
    if !missing.is_empty() {
        finish_embed(ctx, missing_permissions_embed(&missing)).await;
        return Ok(());
    }
    let preview = restore_preview(&view, &snapshot, &mode, job.as_ref(), Some(&row.id), ctx.app.clock().now_ms());
    let info = BackupInfo::of(&row);
    finish(ctx, Payload::embed(build_preview_embed(&info, &preview)).components(vec![restore_buttons(&row.id, &mode)?])).await;
    Ok(())
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let res = match sub.as_str() {
        "create" => run_create(&ctx).await,
        "list" => run_list(&ctx).await,
        "info" => run_info(&ctx).await,
        "export" => run_export(&ctx).await,
        "status" => run_status(&ctx).await,
        "delete" => run_delete(&ctx).await,
        "restore" => run_restore(&ctx).await,
        _ => Ok(()),
    };
    if let Err(e) = res {
        bot_util::log_error!(format!("[backups] {}: /backup {sub} failed", guild_id_of(&ctx)), bot_core::error::log_arg(&e));
        finish_embed(&ctx, failed(if sub == "restore" { "Restore" } else { "Finish" })).await;
    }
    Ok(())
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    if name != "backup" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    match search_backups(ctx.app.db(), &guild_id, &value).await {
        Ok(rows) => {
            let choices: Vec<Choice> = rows.iter().map(|r| Choice::new(backup_label(r), r.id.clone())).collect();
            respond_autocomplete(&ctx.responder, &choices).await;
        }
        Err(e) => {
            bot_util::log_debug!(format!("[backups] autocomplete failed: {e}"));
            respond_autocomplete(&ctx.responder, &[]).await;
        }
    }
    Ok(())
}
