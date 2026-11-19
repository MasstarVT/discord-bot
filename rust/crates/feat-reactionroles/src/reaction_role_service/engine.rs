//! The Discord side of `reactionRoleService.js`: fetching and re-rendering panel messages, the bot's reactions, the
//! role checks and changes, the prune of deleted roles, the member-facing role change (`runPlan`), and the in-process
//! state (member lock, throttles, reaction suppression, background work).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_core::App;
use indexmap::IndexMap;

use super::discord::{ChannelInfo, DErr, MessageInfo, RrDiscord, discord_ops};
use super::index::invalidate_panel_index;
use super::pure::{
    IndexItem, Plan, PlanReason, RATE_LIMIT_MAX, RATE_LIMIT_WINDOW_MS, SUPPRESS_TTL_MS, build_panel_payload, component_emoji, describe_discord_error,
    emoji_key_of_stored, mentions,
};
use super::store::{Panel, clear_panel_missing, delete_items_and_renumber, flag_panel_missing, get_panel, items_of_role};

const SUPPRESS_MAX: usize = 5_000;

// ── In-process state ──────────────────────────────────────────────────────────

/// The per-app state of reaction roles (the JS module-level variables).
pub struct RrState {
    /// `memberMutex`: one role change at a time per `guildId:userId`.
    pub member_mutex: bot_core::sync::KeyedMutex,
    /// `warnThrottle` (10 min).
    pub warn_throttle: bot_util::async_util::Throttle,
    /// `restoreThrottle` (60 s).
    pub restore_throttle: bot_util::async_util::Throttle,
    /// `suppressed`: `${messageId}:${userId}:${key}` -> expiry (ms).
    suppressed: Mutex<IndexMap<String, i64>>,
    /// `background`: fire-and-forget work, so tests can wait for it.
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for RrState {
    fn default() -> Self {
        Self {
            member_mutex: bot_core::sync::KeyedMutex::new(),
            warn_throttle: bot_util::async_util::create_throttle(10 * 60_000),
            restore_throttle: bot_util::async_util::create_throttle(60_000),
            suppressed: Mutex::new(IndexMap::new()),
            background: Mutex::new(Vec::new()),
        }
    }
}

pub fn rr_state(app: &App) -> Arc<RrState> {
    app.ext::<RrState>()
}

/// `runInBackground(label, fn)`: runs `fut` on its own task; a failure is logged.
pub fn run_in_background<F>(app: &App, label: &'static str, fut: F)
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        if let Err(e) = fut.await {
            bot_util::log_error!(format!("reaction-roles {label} failed"), bot_core::error::log_arg(&e));
        }
    });
    let s = rr_state(app);
    let mut bg = s.background.lock().unwrap_or_else(|e| e.into_inner());
    bg.retain(|h| !h.is_finished());
    bg.push(handle);
}

/// `__idle()`: resolves when every background task has finished.
pub async fn idle(app: &App) {
    loop {
        let handles: Vec<_> = {
            let s = rr_state(app);
            let mut bg = s.background.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *bg)
        };
        if handles.is_empty() {
            return;
        }
        for h in handles {
            let _ = h.await;
        }
    }
}

/// `suppressReactionRemoval(messageId, userId, key)`: marks a reaction the bot is about to remove from a member, so
/// the resulting remove event runs no role logic.
pub fn suppress_reaction_removal(app: &App, message_id: &str, user_id: &str, key: &str) {
    let k = format!("{message_id}:{user_id}:{key}");
    let now = app.clock().now_ms();
    let s = rr_state(app);
    let mut m = s.suppressed.lock().unwrap_or_else(|e| e.into_inner());
    m.shift_remove(&k);
    m.insert(k, now + SUPPRESS_TTL_MS);
    while m.len() > SUPPRESS_MAX {
        m.shift_remove_index(0);
    }
}

/// `consumeReactionSuppression(messageId, userId, key)`: true once when the bot removed this reaction itself (and the
/// mark has not expired); consuming deletes it.
pub fn consume_reaction_suppression(app: &App, message_id: &str, user_id: &str, key: &str) -> bool {
    let k = format!("{message_id}:{user_id}:{key}");
    let now = app.clock().now_ms();
    let s = rr_state(app);
    let mut m = s.suppressed.lock().unwrap_or_else(|e| e.into_inner());
    match m.shift_remove(&k) {
        Some(expires) => expires > now,
        None => false,
    }
}

/// `overRateLimit(guildId, userId)`: Redis down fails OPEN (the change is allowed).
pub async fn over_rate_limit(app: &App, guild_id: &str, user_id: &str) -> bool {
    let n = app.cache().sliding_window(&rate_key(guild_id, user_id), RATE_LIMIT_WINDOW_MS, None).await;
    n.is_some_and(|n| n > RATE_LIMIT_MAX)
}

/// The rate-limit key (`rr:rate:{guildId}:{userId}`).
pub fn rate_key(guild_id: &str, user_id: &str) -> String {
    format!("rr:rate:{guild_id}:{user_id}")
}

// ── Panel messages ────────────────────────────────────────────────────────────

/// `markPanelMissing(guild, group)`: flags a panel whose message or channel is gone, only while the guild is
/// available (rule 20). Never fails.
pub async fn mark_panel_missing(app: &App, guild_id: &str, panel_id: &str) {
    let ops = discord_ops(app);
    if !ops.guild_available(guild_id) || panel_id.is_empty() {
        return;
    }
    if let Err(e) = flag_panel_missing(app.db(), guild_id, panel_id).await {
        bot_util::log_warn!(format!("reaction-roles: could not flag panel {panel_id} as missing"), e.to_string());
    }
}

/// The result of [`fetch_panel_message`].
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    Ok { channel: ChannelInfo, message: MessageInfo },
    Missing { code: Option<i64>, reason: String },
    Failed { code: Option<i64>, reason: String },
}

impl Fetched {
    pub fn status(&self) -> &'static str {
        match self {
            Fetched::Ok { .. } => "ok",
            Fetched::Missing { .. } => "missing",
            Fetched::Failed { .. } => "failed",
        }
    }
}

/// `fetchPanelMessage(guild, group)`: the panel's channel and message with REST fallbacks. A cache miss alone never
/// counts as a deletion: only REST's Unknown Channel / Unknown Message flags the panel.
pub async fn fetch_panel_message(app: &App, guild_id: &str, panel_id: &str, channel_id: &str, message_id: &str) -> Fetched {
    let ops = discord_ops(app);
    let channel = match ops.cached_channel(guild_id, channel_id) {
        Some(c) => c,
        None => match ops.fetch_channel(guild_id, channel_id).await {
            Ok(c) => c,
            Err(e) if e.code == Some(10003) => {
                mark_panel_missing(app, guild_id, panel_id).await;
                return Fetched::Missing { code: Some(10003), reason: describe_discord_error(Some(10003)) };
            }
            Err(e) => return Fetched::Failed { code: e.code, reason: describe_discord_error(e.code) },
        },
    };
    if !channel.text_based {
        return Fetched::Failed { code: None, reason: "that is not a text channel".into() };
    }
    match ops.fetch_message(guild_id, channel_id, message_id).await {
        Ok(message) => Fetched::Ok { channel, message },
        Err(e) if e.code == Some(10008) => {
            mark_panel_missing(app, guild_id, panel_id).await;
            Fetched::Missing { code: e.code, reason: describe_discord_error(e.code) }
        }
        Err(e) => Fetched::Failed { code: e.code, reason: describe_discord_error(e.code) },
    }
}

/// [`fetch_panel_message`] for a panel row.
pub async fn fetch_panel(app: &App, guild_id: &str, panel: &Panel) -> Fetched {
    fetch_panel_message(app, guild_id, &panel.group.id, &panel.group.channel_id, &panel.group.message_id).await
}

/// The result of [`sync_panel`] (`{ status, code?, reason? }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncResult {
    /// `ok`, `skipped`, `missing` or `failed`.
    pub status: &'static str,
    pub code: Option<i64>,
    pub reason: Option<String>,
}

impl SyncResult {
    fn of(status: &'static str) -> Self {
        Self { status, code: None, reason: None }
    }
}

/// A panel by id or as a loaded row.
pub enum PanelArg {
    Id(String),
    Row(Box<Panel>),
}

/// `syncPanel(guild, groupOrId, { settings })`: re-renders a panel's message. Attached messages are never edited.
/// `color` is `settings.embedColor`.
pub async fn sync_panel(app: &App, guild_id: &str, panel: PanelArg, color: Option<i64>) -> SyncResult {
    let group = match panel {
        PanelArg::Row(p) => *p,
        PanelArg::Id(id) => match get_panel(app.db(), guild_id, &id).await {
            Ok(Some(p)) => p,
            Ok(None) => return SyncResult::of("skipped"),
            Err(e) => {
                bot_util::log_warn!(format!("reaction-roles: syncPanel failed in {guild_id}"), e.to_string());
                return SyncResult { status: "failed", code: None, reason: Some(describe_discord_error(None)) };
            }
        },
    };
    if group.group.attached {
        return SyncResult::of("skipped");
    }
    let fm = fetch_panel(app, guild_id, &group).await;
    let message = match fm {
        Fetched::Ok { message, .. } => message,
        Fetched::Missing { code, reason } => return SyncResult { status: "missing", code, reason: Some(reason) },
        Fetched::Failed { code, reason } => return SyncResult { status: "failed", code, reason: Some(reason) },
    };
    let view = group.view();
    if super::pure::panel_payload_throws(&view) {
        // The JS buildPanelPayload throws here (an empty embed description): caught by syncPanel, code null.
        bot_util::log_warn!(format!("reaction-roles: syncPanel failed in {guild_id}: the panel has an empty description"));
        return SyncResult { status: "failed", code: None, reason: Some(describe_discord_error(None)) };
    }
    let payload = build_panel_payload(&view, color);
    if let Err(e) = discord_ops(app).edit_message(&message.channel_id, &message.id, &payload.to_api_body()).await {
        if e.code == Some(10008) {
            mark_panel_missing(app, guild_id, &group.group.id).await;
            return SyncResult { status: "missing", code: e.code, reason: Some(describe_discord_error(e.code)) };
        }
        bot_util::log_warn!(format!("reaction-roles: could not edit panel {} in {guild_id}: {}", group.group.id, e.message));
        return SyncResult { status: "failed", code: e.code, reason: Some(describe_discord_error(e.code)) };
    }
    if group.group.message_missing_at.is_some() {
        let _ = clear_panel_missing(app.db(), guild_id, &group.group.id).await;
    }
    SyncResult::of("ok")
}

// ── Reactions ─────────────────────────────────────────────────────────────────

/// The result of [`add_panel_reaction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactResult {
    pub ok: bool,
    pub code: Option<i64>,
    pub reason: Option<String>,
}

/// `addPanelReaction(message, stored)`: adds the bot's reaction.
pub async fn add_panel_reaction(app: &App, message: &MessageInfo, stored: &str) -> ReactResult {
    match discord_ops(app).react(&message.channel_id, &message.id, stored).await {
        Ok(()) => ReactResult { ok: true, code: None, reason: None },
        Err(e) => ReactResult { ok: false, code: e.code, reason: Some(describe_discord_error(e.code)) },
    }
}

/// A reaction [`ensure_panel_reactions`] could not add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactFailure {
    pub emoji: String,
    pub code: Option<i64>,
    pub reason: String,
}

/// The result of [`ensure_panel_reactions`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnsureResult {
    pub added: usize,
    pub failed: Vec<ReactFailure>,
}

/// `ensurePanelReactions(message, items)`: makes sure the bot reacted with every item emoji, in position order.
pub async fn ensure_panel_reactions(app: &App, message: &MessageInfo, items: &[IndexItem]) -> EnsureResult {
    let mut out = EnsureResult::default();
    for item in items {
        let Some(emoji) = item.emoji.as_deref() else { continue };
        if component_emoji(Some(emoji)).is_none() {
            continue;
        }
        let key = item.key.clone().or_else(|| emoji_key_of_stored(Some(emoji))).unwrap_or_default();
        if message.reaction(&key).is_some_and(|r| r.me) {
            continue;
        }
        let r = add_panel_reaction(app, message, emoji).await;
        if r.ok {
            out.added += 1;
        } else {
            out.failed.push(ReactFailure { emoji: emoji.to_string(), code: r.code, reason: r.reason.unwrap_or_else(|| "unknown error".into()) });
        }
    }
    out
}

/// The result of [`remove_panel_reaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveResult {
    pub ok: bool,
    pub all: bool,
}

/// `removePanelReaction(message, stored, { client })`: removes an item's reaction from a panel message: everyone's
/// with Manage Messages, otherwise only the bot's own.
pub async fn remove_panel_reaction(app: &App, guild_id: &str, message: &MessageInfo, stored: &str) -> RemoveResult {
    let ops = discord_ops(app);
    let key = emoji_key_of_stored(Some(stored)).unwrap_or_default();
    let Some(reaction) = message.reaction(&key).cloned() else { return RemoveResult { ok: true, all: false } };
    let res: Result<bool, DErr> = async {
        if ops.bot_has_manage_messages(guild_id, &message.channel_id) {
            ops.remove_emoji_reactions(&message.channel_id, &message.id, &reaction).await?;
            return Ok(true);
        }
        let bot = ops.bot_id().ok_or_else(|| DErr::other("no bot user"))?;
        ops.remove_user_reaction(&message.channel_id, &message.id, &reaction, &bot).await?;
        Ok(false)
    }
    .await;
    match res {
        Ok(all) => RemoveResult { ok: true, all },
        Err(e) => {
            bot_util::log_debug!(format!("reaction-roles: could not remove a reaction: {}", e.message));
            RemoveResult { ok: false, all: false }
        }
    }
}

/// `removeOwnReactions(message, items, client)`: removes only the bot's own reactions for these items (attached
/// panels: never anyone else's).
pub async fn remove_own_reactions(app: &App, message: &MessageInfo, items: &[IndexItem]) {
    let ops = discord_ops(app);
    for item in items {
        let key = item.key.clone().or_else(|| emoji_key_of_stored(item.emoji.as_deref())).unwrap_or_default();
        let Some(reaction) = message.reaction(&key).filter(|r| r.me).cloned() else { continue };
        let Some(bot) = ops.bot_id() else { continue };
        if let Err(e) = ops.remove_user_reaction(&message.channel_id, &message.id, &reaction, &bot).await {
            bot_util::log_debug!(format!("reaction-roles: could not remove an own reaction: {}", e.message));
        }
    }
}

/// `removeMemberReaction(msg, key, userId)`: takes a member's reaction back. Never fails.
pub async fn remove_member_reaction(app: &App, message: Option<&MessageInfo>, key: &str, user_id: &str) {
    let Some(msg) = message else { return };
    let Some(reaction) = msg.reaction(key).cloned() else { return };
    if let Err(e) = discord_ops(app).remove_user_reaction(&msg.channel_id, &msg.id, &reaction, user_id).await {
        bot_util::log_debug!(format!("reaction-roles: could not take back a reaction: {}", e.message));
    }
}

// ── Roles ─────────────────────────────────────────────────────────────────────

/// The result of [`check_roles_usable`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RolesUsable {
    pub missing: Vec<String>,
    pub blocked: Vec<String>,
    pub no_manage_roles: bool,
}

/// `checkRolesUsable(guild, roleIds)`: which of these roles the bot cannot use right now. `missing` comes from the
/// role cache, so it is only a suspicion: confirm with REST before treating a role as deleted (rule 20).
pub fn check_roles_usable(ops: &dyn RrDiscord, guild_id: &str, role_ids: &[String]) -> RolesUsable {
    let no_manage_roles = !ops.bot_can_manage_roles(guild_id);
    let mut out = RolesUsable { no_manage_roles, ..Default::default() };
    let mut seen = HashSet::new();
    for id in role_ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        match ops.cached_role(guild_id, id) {
            None => out.missing.push(id.clone()),
            Some(r) if no_manage_roles || !r.editable => out.blocked.push(id.clone()),
            Some(_) => {}
        }
    }
    out
}

/// `roleVerdict(guild, roleId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleVerdict {
    Exists,
    /// Only for a REST "no such role" while the guild is available.
    Gone,
    Unknown,
}

pub async fn role_verdict(ops: &dyn RrDiscord, guild_id: &str, role_id: &str) -> RoleVerdict {
    if !ops.guild_available(guild_id) {
        return RoleVerdict::Unknown;
    }
    match ops.fetch_role_exists(guild_id, role_id).await {
        Ok(true) => RoleVerdict::Exists,
        Ok(false) => RoleVerdict::Gone,
        Err(_) => RoleVerdict::Unknown,
    }
}

/// The result of [`verify_roles`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifiedRoles {
    pub gone: Vec<String>,
    pub unverified: Vec<String>,
    pub blocked: Vec<String>,
    pub no_manage_roles: bool,
}

/// `verifyRoles(guild, roleIds)`: [`check_roles_usable`] with the cache-miss suspicions confirmed over REST.
pub async fn verify_roles(ops: &dyn RrDiscord, guild_id: &str, role_ids: &[String]) -> VerifiedRoles {
    let mut u = check_roles_usable(ops, guild_id, role_ids);
    let mut gone = Vec::new();
    if !u.missing.is_empty() {
        for id in &u.missing {
            if role_verdict(ops, guild_id, id).await == RoleVerdict::Gone {
                gone.push(id.clone());
            }
        }
        u = check_roles_usable(ops, guild_id, role_ids); // roles that exist are cached by the fetch
    }
    let unverified = u.missing.iter().filter(|id| !gone.contains(id)).cloned().collect();
    VerifiedRoles { gone, unverified, blocked: u.blocked, no_manage_roles: u.no_manage_roles }
}

/// A failed role call of [`apply_role_change`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFailure {
    pub role_id: String,
    /// `add` or `remove`.
    pub op: &'static str,
    pub code: Option<i64>,
}

/// The result of [`apply_role_change`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Applied {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub failed: Option<RoleFailure>,
}

/// `applyRoleChange(member, plan, { reason })`: one role id at a time, adds first (a failed add stops everything, so a
/// failed swap keeps the old role), then removes. `held` is the member's role ids as read (the cached member).
pub async fn apply_role_change(ops: &dyn RrDiscord, guild_id: &str, user_id: &str, held: &[String], plan: &Plan, reason: &str) -> Applied {
    let mut out = Applied::default();
    for role_id in &plan.to_add {
        if held.contains(role_id) {
            continue;
        }
        match ops.add_role(guild_id, user_id, role_id, reason).await {
            Ok(()) => out.added.push(role_id.clone()),
            Err(e) => {
                out.failed = Some(RoleFailure { role_id: role_id.clone(), op: "add", code: e.code });
                return out;
            }
        }
    }
    for role_id in &plan.to_remove {
        if !held.contains(role_id) {
            continue;
        }
        match ops.remove_role(guild_id, user_id, role_id, reason).await {
            Ok(()) => out.removed.push(role_id.clone()),
            Err(e) => {
                out.failed = Some(RoleFailure { role_id: role_id.clone(), op: "remove", code: e.code });
                return out;
            }
        }
    }
    out
}

/// `pruneItemsForRole(guild, roleId, { client })`: deletes the items of a deleted role (after a `roleDelete` or a
/// confirmed 10011) and re-renders the panels in the background. Never fails; returns how many items were removed.
pub async fn prune_items_for_role(app: &App, guild_id: &str, role_id: &str) -> usize {
    let ops = discord_ops(app);
    if !ops.guild_available(guild_id) {
        return 0;
    }
    let res: anyhow::Result<usize> = async {
        let items = items_of_role(app.db(), guild_id, role_id).await?;
        if items.is_empty() {
            return Ok(0);
        }
        let mut group_ids: Vec<String> = Vec::new();
        for i in &items {
            if !group_ids.contains(&i.group_id) {
                group_ids.push(i.group_id.clone());
            }
        }
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        delete_items_and_renumber(app.db(), &ids, &group_ids).await?;
        invalidate_panel_index(app, guild_id).await;

        let (bg_app, gid, items_bg, groups) = (app.clone(), guild_id.to_string(), items.clone(), group_ids.clone());
        run_in_background(app, "prune re-sync", async move {
            let color = bg_app.settings().get_settings_safe(&gid).await.and_then(|s| s.embed_color).map(i64::from);
            let jobs: Vec<String> = groups;
            let app2 = bg_app.clone();
            bot_util::async_util::map_limit(jobs, 2, move |group_id: String, _i: usize| {
                let (app, gid, items) = (app2.clone(), gid.clone(), items_bg.clone());
                async move {
                    let Some(group) = get_panel(app.db(), &gid, &group_id).await? else { return Ok::<(), anyhow::Error>(()) };
                    sync_panel(&app, &gid, PanelArg::Row(Box::new(group.clone())), color).await;
                    if group.layout() != "REACTIONS" {
                        return Ok(());
                    }
                    let Fetched::Ok { message, .. } = fetch_panel(&app, &gid, &group).await else { return Ok(()) };
                    for item in items.iter().filter(|i| i.group_id == group_id) {
                        if let Some(e) = item.emoji.as_deref().filter(|e| !e.is_empty()) {
                            remove_panel_reaction(&app, &gid, &message, e).await;
                        }
                    }
                    Ok(())
                }
            })
            .await;
            Ok(())
        });
        bot_util::log_info!(format!("reaction-roles: pruned {} item(s) for deleted role {role_id} in {guild_id}", items.len()));
        Ok(items.len())
    }
    .await;
    match res {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_error!(format!("reaction-roles: could not prune role {role_id} in {guild_id}"), bot_core::error::log_arg(&e));
            0
        }
    }
}

/// Starts [`prune_items_for_role`] in the background.
pub fn prune_in_background(app: &App, guild_id: &str, role_id: &str) {
    let (a, g, r) = (app.clone(), guild_id.to_string(), role_id.to_string());
    run_in_background(app, "prune", async move {
        prune_items_for_role(&a, &g, &r).await;
        Ok(())
    });
}

// ── Member-facing role changes ────────────────────────────────────────────────

/// The member-facing "nothing changed" text.
pub const NO_CHANGES: &str = "No changes: your roles already match your choice.";

/// What [`run_plan`] needs to know of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPanel {
    pub title: String,
    pub max_roles: Option<i32>,
}

/// The result of [`run_plan`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanOutcome {
    pub content: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn fail_text(code: Option<i64>) -> &'static str {
    match code {
        Some(50013) => "I'm missing permission to manage it",
        Some(10011) => "it no longer exists",
        _ => "something went wrong",
    }
}

/// `runPlan({ guild, member, group, plan, client, targetRoleIds })`: checks, applies and reports one plan. Returns the
/// member-facing text plus what changed. `held` is the member's role ids as read.
pub async fn run_plan(app: &App, guild_id: &str, user_id: &str, held: &[String], panel: &PlanPanel, plan: &Plan, target_role_ids: &[String]) -> PlanOutcome {
    let ops = discord_ops(app);
    let nothing = |content: String| PlanOutcome { content, added: Vec::new(), removed: Vec::new() };
    if let Some(reason) = plan.reason {
        let content = match reason {
            PlanReason::Max => {
                format!("You can hold at most **{}** role(s) from this panel. Remove one first.", panel.max_roles.map(|m| m.to_string()).unwrap_or_else(|| "null".into()))
            }
            PlanReason::Already => {
                if target_role_ids.len() == 1 {
                    format!("You already have <@&{}>.", target_role_ids[0])
                } else {
                    "You already have these roles.".to_string()
                }
            }
            PlanReason::NotHeld => {
                if target_role_ids.is_empty() {
                    "You don't have those roles, so there's nothing to remove.".to_string()
                } else {
                    format!("You don't have {}, so there's nothing to remove.", mentions(target_role_ids))
                }
            }
            PlanReason::NoChange => NO_CHANGES.to_string(),
        };
        return nothing(content);
    }

    let all: Vec<String> = plan.to_add.iter().chain(plan.to_remove.iter()).cloned().collect();
    let v = verify_roles(ops.as_ref(), guild_id, &all).await;
    for id in &v.gone {
        prune_in_background(app, guild_id, id);
    }
    let lost: HashSet<&String> = v.gone.iter().chain(v.unverified.iter()).collect();
    if plan.to_add.iter().any(|id| v.gone.contains(id)) {
        return nothing("That role no longer exists, so I removed it from this panel.".into());
    }
    if plan.to_add.iter().any(|id| lost.contains(id)) {
        return nothing("I couldn't check that role right now. Please try again in a moment.".into());
    }
    let usable = Plan { to_remove: plan.to_remove.iter().filter(|id| !lost.contains(id)).cloned().collect(), ..plan.clone() };
    if !v.blocked.is_empty() {
        return nothing(if v.no_manage_roles {
            "I need the **Manage Roles** permission to give roles. Ask an admin to fix my permissions.".into()
        } else {
            format!("I can't manage {} because it's above my highest role. Ask an admin to move my role higher.", mentions(&v.blocked))
        });
    }
    if usable.to_add.is_empty() && usable.to_remove.is_empty() {
        return nothing(NO_CHANGES.into());
    }

    let res = apply_role_change(ops.as_ref(), guild_id, user_id, held, &usable, &super::pure::role_reason(&panel.title)).await;
    let mut lines = Vec::new();
    if !res.added.is_empty() {
        lines.push(format!("✅ Added: {}", mentions(&res.added)));
    }
    if !res.removed.is_empty() {
        lines.push(format!("➖ Removed: {}", mentions(&res.removed)));
    }
    if !plan.skipped.is_empty() {
        lines.push(format!(
            "⚠️ Not added (limit of **{}**): {}",
            panel.max_roles.map(|m| m.to_string()).unwrap_or_else(|| "null".into()),
            mentions(&plan.skipped)
        ));
    }
    if let Some(f) = &res.failed {
        lines.push(format!("⚠️ I couldn't {} <@&{}>: {}.", f.op, f.role_id, fail_text(f.code)));
        if f.code == Some(10011) && role_verdict(ops.as_ref(), guild_id, &f.role_id).await == RoleVerdict::Gone {
            prune_in_background(app, guild_id, &f.role_id);
        }
    }
    if lines.is_empty() {
        lines.push(NO_CHANGES.to_string());
    }
    PlanOutcome { content: bot_util::text::clamp_text(&lines.join("\n"), 2000), added: res.added, removed: res.removed }
}

/// `executePlan(args)`: [`run_plan`]'s member-facing text.
pub async fn execute_plan(app: &App, guild_id: &str, user_id: &str, held: &[String], panel: &PlanPanel, plan: &Plan, target_role_ids: &[String]) -> String {
    run_plan(app, guild_id, user_id, held, panel, plan, target_role_ids).await.content
}
