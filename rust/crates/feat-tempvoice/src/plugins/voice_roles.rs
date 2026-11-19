//! Port of `src/plugins/voice-roles.js`: members have a role while they are in a linked voice channel (or a voice
//! channel of a linked category), and the "any voice channel" roles while they are in voice at all. Each voice
//! change recomputes the member's roles from their current voice state; guildReady and `/voicerole sync` reconcile
//! everyone. While the module is off nothing changes any role. Rules: [`crate::voice_role_service`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_core::prelude::*;
use bot_core::settings::is_module_enabled;
use bot_util::{log_error, log_info};
use serenity::all::GuildId;

use crate::voice_role_service::{
    ApplyOptions, RECONCILE_MAX, apply_member, has_voice_roles, is_reconciling, links_of, patch_drop_channel, patch_drop_role, reconcile_guild,
    release_dropped_roles, voice_of,
};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The plugin's module-level state: the startup queue (one guild at a time) and the detached role releases.
pub struct VoiceRolesPluginState {
    startup_queue: bot_util::async_util::Semaphore,
    startup_queued: Mutex<HashSet<String>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for VoiceRolesPluginState {
    fn default() -> Self {
        Self { startup_queue: bot_util::async_util::create_semaphore(1), startup_queued: Mutex::default(), tasks: Mutex::default() }
    }
}

fn track(app: &App, handle: tokio::task::JoinHandle<()>) {
    let st = app.ext::<VoiceRolesPluginState>();
    let mut tasks = lock(&st.tasks);
    tasks.retain(|h| !h.is_finished());
    tasks.push(handle);
}

/// Resolves once every startup sync and role release started so far has finished (`__startupSyncsIdle` and
/// `__releasesIdle`).
pub async fn idle(app: &App) {
    loop {
        let pending: Vec<_> = std::mem::take(&mut *lock(&app.ext::<VoiceRolesPluginState>().tasks));
        if pending.is_empty() {
            return;
        }
        for h in pending {
            let _ = h.await;
        }
    }
}

// ── Filters (sync, cheap) ─────────────────────────────────────────────────────

/// `voiceFilter(ctx)`: only a join, leave or move matters, bots never get voice roles; with a memo showing the module
/// off or nothing set up the hook is skipped too.
pub fn voice_filter(app: &App, guild_id: GuildId, change: VoiceChange, member_is_bot: bool) -> bool {
    if change == VoiceChange::State || member_is_bot {
        return false;
    }
    let Some(s) = app.settings().peek_settings(&guild_id.to_string()) else { return true };
    is_module_enabled(Some(&s), Some(ModuleKey::Voiceroles)) && has_voice_roles(Some(&s))
}

/// `channelDeleteFilter(ctx)`: skipped once the memo shows no link uses the channel.
pub fn channel_delete_filter(app: &App, guild_id: GuildId, channel_id: &str) -> bool {
    let Some(s) = app.settings().peek_settings(&guild_id.to_string()) else { return true };
    s.voice_role_links.as_array().is_some_and(|a| a.iter().any(|l| l.get("channelId").and_then(serde_json::Value::as_str) == Some(channel_id)))
}

/// `roleDeleteFilter(ctx)`: skipped once the memo shows the role is in no link and not in the any-list.
pub fn role_delete_filter(app: &App, guild_id: GuildId, role_id: &str) -> bool {
    let Some(s) = app.settings().peek_settings(&guild_id.to_string()) else { return true };
    s.voice_role_links.as_array().is_some_and(|a| a.iter().any(|l| l.get("roleId").and_then(serde_json::Value::as_str) == Some(role_id)))
        || s.voice_role_any_role_ids.iter().any(|id| id == role_id)
}

// ── Voice changes ─────────────────────────────────────────────────────────────

/// `onVoiceStateUpdate(ctx)`: recomputes the member's voice roles from their new voice state. Never fails.
pub async fn on_voice_state_update(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !has_voice_roles(Some(&settings)) {
        return Ok(());
    }
    let guild = bot_core::interaction_helpers::cached_guild(&ctx.app, ctx.data.guild_id);
    let voice = voice_of(Some(&ctx.data.new_state), guild.as_ref());
    apply_member(&ctx.app, ctx.data.guild_id, &ctx.data.member, Some(settings), ApplyOptions { voice, ..Default::default() }).await;
    Ok(())
}

// ── Startup reconcile ─────────────────────────────────────────────────────────

/// `onGuildReady(ctx)`: reconciles the guild, detached, one guild at a time, so the role calls never hold up the
/// later guildReady hooks or the next guild's startup.
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !has_voice_roles(Some(&settings)) {
        return Ok(());
    }
    let app = ctx.app.clone();
    let guild_id = ctx.data.guild_id;
    let g = guild_id.to_string();
    let st = app.ext::<VoiceRolesPluginState>();
    if lock(&st.startup_queued).contains(&g) || is_reconciling(&app, &g) {
        return Ok(());
    }
    lock(&st.startup_queued).insert(g.clone());
    let (app2, st2) = (app.clone(), st.clone());
    let queued = async move {
        let st3 = st2.clone();
        st2.startup_queue
            .run(move || async move {
                let (app, st) = (app2, st3);
            lock(&st.startup_queued).remove(&g);
            // The queue can make this wait behind other guilds: look at the settings again.
            let Some(fresh) = app.settings().get_settings_safe(&g).await else { return };
            if !is_module_enabled(Some(&fresh), Some(ModuleKey::Voiceroles)) || !has_voice_roles(Some(&fresh)) {
                return;
            }
            let res = reconcile_guild(&app, guild_id, Some(fresh), RECONCILE_MAX).await;
            if res.changed > 0 || res.failed > 0 {
                log_info!(format!(
                    "[voice-roles] {g}: startup sync checked {}, gave {} and took {} role(s), {} failed",
                    res.checked, res.added, res.removed, res.failed
                ));
            }
            })
            .await;
    };
    let handle = tokio::spawn(async move {
        // The semaphore future borrows nothing; a panic inside is reported like the JS `.catch`.
        if let Err(e) = tokio::spawn(queued).await {
            log_error!(format!("[voice-roles] startup sync failed for {guild_id}"), e.to_string());
        }
    });
    track(&app, handle);
    Ok(())
}

// ── Cleanup (runs while the module is off, too) ───────────────────────────────

/// `onChannelDelete(ctx)`: drops the links of the deleted channel (or category). While the module is on, the roles of
/// those links are then taken from members who hold them and are not entitled to them any more (detached).
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let channel_id = ctx.data.channel.id.to_string();
    let guild_id = ctx.data.guild_id;
    let dropped = Arc::new(Mutex::new(Vec::<String>::new()));
    let (d, ch) = (dropped.clone(), channel_id.clone());
    let next = ctx
        .app
        .settings()
        .update_settings_with(&guild_id.to_string(), move |cur| {
            let patch = patch_drop_channel(&cur, &ch);
            *lock(&d) = if patch.is_some() { links_of(Some(&cur)).into_iter().filter(|l| l.channel_id == ch).map(|l| l.role_id).collect() } else { Vec::new() };
            Ok(patch.unwrap_or_default())
        })
        .await?;
    let dropped_ids = lock(&dropped).clone();
    if dropped_ids.is_empty() || !is_module_enabled(Some(&next), Some(ModuleKey::Voiceroles)) {
        return Ok(());
    }
    let app = ctx.app.clone();
    let handle = tokio::spawn(async move {
        let res = release_dropped_roles(&app, guild_id, Some(next), &dropped_ids, RECONCILE_MAX).await;
        if res.removed > 0 || res.failed > 0 {
            log_info!(format!("[voice-roles] {guild_id}: {channel_id} was deleted, took its role(s) from {} member(s), {} failed", res.removed, res.failed));
        }
    });
    track(&ctx.app, handle);
    Ok(())
}

/// `onRoleDelete(ctx)`: drops the deleted role from the links and the any-list.
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    let role_id = ctx.data.role_id.to_string();
    ctx.app.settings().update_settings_with(&ctx.data.guild_id.to_string(), move |cur| Ok(patch_drop_role(&cur, &role_id).unwrap_or_default())).await?;
    Ok(())
}

/// The `voice-roles` plugin.
pub fn plugin() -> Plugin {
    Plugin::new("voice-roles", Some(ModuleKey::Voiceroles))
        // A fast hop can queue several role calls behind each other, so the soft timeout is generous.
        .hook(
            Hook::<VoiceStateUpdate>::new("voice-roles", on_voice_state_update)
                .filter(|ctx| voice_filter(&ctx.app, ctx.data.guild_id, ctx.data.change, ctx.data.member.user.bot))
                .timeout_ms(30_000),
        )
        .hook(Hook::<GuildReady>::new("voice-roles", on_guild_ready))
        // Cleanup runs even while the module is off, so a deleted channel or role never lingers in the settings.
        .hook(
            Hook::<ChannelDelete>::new("voice-roles", on_channel_delete)
                .always()
                .filter(|ctx| channel_delete_filter(&ctx.app, ctx.data.guild_id, &ctx.data.channel.id.to_string())),
        )
        .hook(
            Hook::<RoleDelete>::new("voice-roles", on_role_delete)
                .always()
                .filter(|ctx| role_delete_filter(&ctx.app, ctx.data.guild_id, &ctx.data.role_id.to_string())),
        )
}
