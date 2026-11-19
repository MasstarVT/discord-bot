//! Port of `src/plugins/role-persist.js`: when a member leaves, their roles (and nickname) are saved; when they rejoin
//! within `rolePersistDays` the roles that are still safe to give are given back, one single-role call each with an
//! audit-log reason. Never bypasses Rules Screening or the captcha: while either is open the restore waits and runs
//! from guildMemberUpdate once it clears. Rules and table access: [`crate::role_persist_service`].

use std::sync::Arc;

use bot_core::interaction_helpers::with_cached_guild;
use bot_core::prelude::*;
use bot_db::GuildSettings;
use serenity::all::{GuildId, Member, Permissions, RoleId};

use crate::role_persist_service::{
    self as rp, Gate, Skipped, clear_event_markers, clear_waiting, consts as RP, delete_row, gate_of, is_stale, is_waiting, lifecycle_skips, load_row,
    mark_waiting, plan_roles, prune_expired, run_locked, save_row, snapshot_member,
};

/// `AUDIT_REASON`.
pub const AUDIT_REASON: &str = "Role Persistence: roles from before the member left";

/// "Unknown member" and "Unknown user": the member is gone again, nothing to restore.
const LEFT_CODES: [u64; 2] = [10007, 10013];
/// "Unknown role" and "Missing permissions": this role cannot be given; the others still can.
const ROLE_FAIL_CODES: [u64; 2] = [10011, 50013];

/// The plugin's throttles (`warnBlocked` once an hour per guild, `partialLook` once a minute per member).
pub struct Throttles {
    warn_blocked: std::sync::OnceLock<bot_util::async_util::Throttle>,
    partial_look: std::sync::OnceLock<bot_util::async_util::Throttle>,
}

impl Default for Throttles {
    fn default() -> Self {
        Self { warn_blocked: std::sync::OnceLock::new(), partial_look: std::sync::OnceLock::new() }
    }
}

fn throttles(app: &App) -> Arc<Throttles> {
    app.ext::<Throttles>()
}

fn warn_blocked(app: &App, key: &str) -> bool {
    let t = throttles(app);
    t.warn_blocked.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), 3_600_000)).allow(key)
}

fn partial_look(app: &App, key: &str) -> bool {
    let t = throttles(app);
    t.partial_look.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), 60_000)).allow(key)
}

// ── Filters (sync) ────────────────────────────────────────────────────────────

/// `leaveFilter(ctx)`: people only.
pub fn leave_filter(ctx: &HookCtx<GuildMemberRemove>) -> bool {
    !ctx.data.user.bot
}

/// `joinFilter(ctx)`: people only.
pub fn join_filter(ctx: &HookCtx<GuildMemberAdd>) -> bool {
    !ctx.data.member.user.bot
}

/// `gateMayHaveCleared(ctx)`: only an update that may have cleared a gate is looked at: a member we are waiting for,
/// a Rules Screening pass, the captcha role being gained, or (the old member was not cached) a recent joiner, at most
/// once a minute each.
pub fn gate_may_have_cleared(ctx: &HookCtx<GuildMemberUpdate>) -> bool {
    let n = &ctx.data.member;
    if n.user.bot {
        return false;
    }
    let g = ctx.data.guild_id.to_string();
    let now = ctx.app.clock().now_ms();
    if is_waiting(&ctx.app, &g, &n.user.id.to_string(), now) {
        return true;
    }
    if ctx.data.old_known {
        let o = ctx.data.old_member.as_ref();
        if o.is_some_and(|o| o.pending) && !n.pending {
            return true;
        }
        let Some(settings) = ctx.app.settings().peek_settings(&g) else {
            return n.roles.len() > o.map(|o| o.roles.len()).unwrap_or(0);
        };
        let gate = settings.captcha_enabled.then(|| settings.captcha_role_id.clone()).flatten().and_then(|r| r.parse::<u64>().ok().filter(|v| *v != 0));
        let Some(gate) = gate.map(RoleId::new) else { return false };
        return !o.is_some_and(|o| o.roles.contains(&gate)) && n.roles.contains(&gate);
    }
    let Some(joined) = n.joined_at.map(|t| t.timestamp_millis()) else { return false };
    if now - joined > RP::RECENT_JOIN_MS || n.pending {
        return false;
    }
    partial_look(&ctx.app, &format!("{g}:{}", n.user.id))
}

/// `roleDeleteFilter(ctx)`: skipped once the memoised settings show the role is not on the ignore list.
pub fn role_delete_filter(ctx: &HookCtx<RoleDelete>) -> bool {
    match ctx.app.settings().peek_settings(&ctx.data.guild_id.to_string()) {
        None => true,
        Some(s) => s.role_persist_ignored_role_ids.contains(&ctx.data.role_id.to_string()),
    }
}

// ── Leave ─────────────────────────────────────────────────────────────────────

/// `onLeave(ctx)`: saves the member's roles and nickname, then prunes this guild's expired rows (at most once an
/// hour).
pub async fn on_leave(ctx: Arc<HookCtx<GuildMemberRemove>>) -> BotResult {
    let g = ctx.data.guild_id;
    let gs = g.to_string();
    let user = ctx.data.user.id.to_string();
    clear_waiting(&ctx.app, &gs, &user);
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let snap = with_cached_guild(&ctx.app, g, |guild| snapshot_member(ctx.data.member.as_ref(), guild, Some(&settings))).flatten();
    if let Some(snap) = snap {
        let (app, gs2, u2, s2) = (ctx.app.clone(), gs.clone(), user.clone(), settings.clone());
        let now = ctx.app.clock().now();
        run_locked(&ctx.app, &gs, &user, move || async move { save_row(&app, &gs2, &u2, &snap, now, Some(&s2)).await }).await?;
    }
    prune_expired(&ctx.app, &gs, Some(&settings), ctx.app.clock().now_ms()).await;
    Ok(())
}

// ── Rejoin ────────────────────────────────────────────────────────────────────

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    None,
    Stale,
    Waiting(Gate),
    Blocked,
    Left,
    Failed,
    Restored { given: Vec<String>, skipped: Vec<Skipped>, nickname: bool },
}

impl Restore {
    pub fn status(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Stale => "stale",
            Self::Waiting(_) => "waiting",
            Self::Blocked => "blocked",
            Self::Left => "left",
            Self::Failed => "failed",
            Self::Restored { .. } => "restored",
        }
    }
}

/// `canSetNickname(guild, member)`: the bot has Manage Nicknames and `member.manageable`.
fn can_set_nickname(app: &App, guild_id: GuildId, member: &Member) -> bool {
    let Some(bot) = app.discord().map(|d| d.bot_user_id()) else { return false };
    with_cached_guild(app, guild_id, |g| {
        let Some(me) = bot_core::permissions::me(g, bot) else { return false };
        let perms = bot_core::permissions::guild_permissions(g, bot, &me.roles);
        if !bot_core::permissions::js_has(perms, Permissions::MANAGE_NICKNAMES) {
            return false;
        }
        // discord.js `member.manageable`.
        if member.user.id == g.owner_id || member.user.id == bot {
            return false;
        }
        if bot == g.owner_id {
            return true;
        }
        bot_core::permissions::highest_position(g, &me.roles) > bot_core::permissions::highest_position(g, &member.roles)
    })
    .unwrap_or(false)
}

enum Added {
    Given(Vec<String>),
    Left,
    Failed,
}

/// `addRoles(target, ids)`: one single-role PUT each (a role another hook added meanwhile is never taken off).
async fn add_roles(app: &App, guild_id: GuildId, member: &Member, ids: &[RoleId]) -> Added {
    let Some(http) = app.http() else { return Added::Failed };
    let mut given = Vec::new();
    for id in ids {
        match http.add_member_role(guild_id, member.user.id, *id, Some(AUDIT_REASON)).await {
            Ok(()) => given.push(id.to_string()),
            Err(e) => {
                let code = bot_core::discord::error_code(&e);
                if code.is_some_and(|c| LEFT_CODES.contains(&c)) {
                    return Added::Left;
                }
                if !code.is_some_and(|c| ROLE_FAIL_CODES.contains(&c)) {
                    bot_util::log_warn!(format!(
                        "[role-persist] {guild_id}: could not give role {id} to {}: {}",
                        member.user.id,
                        bot_core::discord::error_message(&e)
                    ));
                    return Added::Failed;
                }
            }
        }
    }
    Added::Given(given)
}

async fn restore_locked(app: &App, guild_id: GuildId, member: &Member, settings: &GuildSettings, captcha_pending: bool, now_ms: i64) -> anyhow::Result<Restore> {
    let g = guild_id.to_string();
    let user = member.user.id.to_string();
    let Some(row) = load_row(app, &g, &user).await? else {
        clear_waiting(app, &g, &user);
        return Ok(Restore::None);
    };
    if is_stale(&row.left_at, Some(settings), now_ms) {
        delete_row(app, &g, &user).await?;
        clear_waiting(app, &g, &user);
        return Ok(Restore::Stale);
    }
    let gate = match with_cached_guild(app, guild_id, |guild| gate_of(Some(guild), member, Some(settings), captcha_pending)) {
        Some(gate) => gate,
        None => gate_of(None, member, Some(settings), captcha_pending),
    };
    if let Some(gate) = gate {
        mark_waiting(app, &g, &user, now_ms);
        return Ok(Restore::Waiting(gate));
    }
    let bot = app.discord().map(|d| d.bot_user_id());
    let can_manage = bot.is_some_and(|b| {
        with_cached_guild(app, guild_id, |guild| {
            bot_core::permissions::me(guild, b)
                .is_some_and(|me| bot_core::permissions::js_has(bot_core::permissions::guild_permissions(guild, b, &me.roles), Permissions::MANAGE_ROLES))
        })
        .unwrap_or(false)
    });
    if !row.role_ids.is_empty() && !can_manage {
        // Fixable by an admin: keep the row (it still expires on its own).
        if warn_blocked(app, &g) {
            bot_util::log_warn!(format!("[role-persist] {g}: cannot restore roles, the bot lacks Manage Roles"));
        }
        return Ok(Restore::Blocked);
    }
    // A mute, event or birthday role may have been closed while the member was away: those are not given back.
    let missing: Vec<String> = row.role_ids.iter().filter(|id| !member.roles.iter().any(|r| r.to_string() == **id)).cloned().collect();
    let life = lifecycle_skips(app, &g, &user, &missing, Some(settings), chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default()).await?;
    let plan = match (bot, with_cached_guild(app, guild_id, |guild| bot.map(|b| plan_roles(guild, b, &row.role_ids, Some(settings), Some(&member.roles), Some(&life.skips))))) {
        (Some(_), Some(Some(p))) => p,
        _ => rp::Plan::default(),
    };

    let mut given = Vec::new();
    if !plan.give.is_empty() {
        match add_roles(app, guild_id, member, &plan.give).await {
            Added::Left => return Ok(Restore::Left),
            Added::Failed => return Ok(Restore::Failed),
            Added::Given(g) => given = g,
        }
    }

    let mut nickname = false;
    if settings.role_persist_nicknames
        && let Some(nick) = row.nickname.as_deref().filter(|n| !n.is_empty())
        && member.nick.as_deref().is_none_or(str::is_empty)
        && can_set_nickname(app, guild_id, member)
        && let Some(http) = app.http()
    {
        match http.edit_member(guild_id, member.user.id, &serde_json::json!({ "nick": nick }), Some(AUDIT_REASON)).await {
            Ok(_) => nickname = true,
            Err(e) => {
                if bot_core::discord::error_code(&e).is_some_and(|c| LEFT_CODES.contains(&c)) {
                    return Ok(Restore::Left);
                }
                bot_util::log_warn!(format!("[role-persist] {g}: could not restore the nickname of {user}: {}", bot_core::discord::error_message(&e)));
            }
        }
    }

    delete_row(app, &g, &user).await?;
    // After the row is gone: a failed delete keeps the flag, so the retry still skips.
    clear_event_markers(app, &user, &life.markers).await;
    clear_waiting(app, &g, &user);
    if !given.is_empty() || nickname {
        bot_util::log_info!(format!(
            "[role-persist] {g}: restored {} role(s){} for {user}",
            given.len(),
            if nickname { " and the nickname" } else { "" }
        ));
    }
    Ok(Restore::Restored { given, skipped: plan.skipped, nickname })
}

/// `restoreMember(member, settings, { captchaPending, now })`: gives a rejoining member their saved roles (and
/// nickname) back, when a fresh row exists and no gate is open. Never fails.
pub async fn restore_member(app: &App, guild_id: GuildId, member: &Member, settings: &GuildSettings, captcha_pending: bool, now_ms: Option<i64>) -> Restore {
    let now = now_ms.unwrap_or_else(|| app.clock().now_ms());
    let (a, m, s) = (app.clone(), member.clone(), settings.clone());
    let res = run_locked(app, &guild_id.to_string(), &member.user.id.to_string(), move || async move {
        restore_locked(&a, guild_id, &m, &s, captcha_pending, now).await
    })
    .await;
    match res {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[role-persist] {guild_id}: restore failed for {}", member.user.id), bot_core::error::log_arg(&e));
            Restore::Failed
        }
    }
}

/// `onJoin(ctx)` (after welcome and captcha): restores unless a gate is open.
pub async fn on_join(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let pending = ctx.state.get::<bool>(feat_welcome::welcome_service::CAPTCHA_PENDING_KEY) == Some(true);
    restore_member(&ctx.app, ctx.data.guild_id, &ctx.data.member, &settings, pending, None).await;
    Ok(())
}

/// `onUpdate(ctx)`: a gate may have cleared, so try the restore again.
pub async fn on_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    restore_member(&ctx.app, ctx.data.guild_id, &ctx.data.member, &settings, false, None).await;
    Ok(())
}

// ── Role cleanup ──────────────────────────────────────────────────────────────

/// `onRoleDelete(ctx)`: drops the role from the never-give-back list.
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    let role = ctx.data.role_id.to_string();
    let settings = ctx.get_settings().await;
    if !settings.is_some_and(|s| s.role_persist_ignored_role_ids.contains(&role)) {
        return Ok(());
    }
    ctx.app
        .settings()
        .update_settings_with(&ctx.data.guild_id.to_string(), move |cur| {
            if !cur.role_persist_ignored_role_ids.contains(&role) {
                return Ok(SettingsPatch::new());
            }
            let list: Vec<String> = cur.role_persist_ignored_role_ids.iter().filter(|id| **id != role).cloned().collect();
            Ok(SettingsPatch::new().set("rolePersistIgnoredRoleIds", serde_json::json!(list)))
        })
        .await?;
    Ok(())
}

/// The plugin (`src/plugins/role-persist.js`).
pub fn plugin() -> Plugin {
    Plugin::new("role-persist", Some(ModuleKey::Rolepersist))
        .hook(Hook::<GuildMemberRemove>::new("role-persist", on_leave).filter(leave_filter))
        .hook(Hook::<GuildMemberAdd>::new("role-persist", on_join).filter(join_filter).timeout_ms(20_000))
        .hook(Hook::<GuildMemberUpdate>::new("role-persist", on_update).filter(gate_may_have_cleared).timeout_ms(20_000))
        // Cleanup runs even while the module is off, so a deleted role never lingers on the ignore list.
        .hook(Hook::<RoleDelete>::new("role-persist", on_role_delete).always().filter(role_delete_filter))
}
