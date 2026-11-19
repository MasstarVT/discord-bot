//! The persistent component handlers (`rr`, `rr-select`) and the hook bodies of `reactionRoleService.js`.

use std::sync::Arc;

use bot_core::App;
use bot_core::interaction_helpers::{Payload, is_interaction_gone, safe_defer, safe_reply};
use bot_core::prelude::*;
use serenity::builder::{CreateAllowedMentions, EditInteractionResponse};

use super::discord::discord_ops;
use super::engine::{
    PlanPanel, add_panel_reaction, apply_role_change, consume_reaction_suppression, ensure_panel_reactions, execute_plan, over_rate_limit,
    prune_in_background, prune_items_for_role, remove_member_reaction, role_verdict, rr_state, run_in_background, run_plan,
    suppress_reaction_removal, verify_roles, RoleVerdict,
};
use super::index::{get_panel_index, peek_panel_index};
use super::pure::{Plan, PlanReason, Source, build_select_row, normalize_emoji_key, plan_role_change, role_reason};
use super::store::{flag_message_panels_missing, get_panel};

/// "This panel is no longer active."
pub const NO_PANEL: &str = "⚠️ This panel is no longer active.";
/// "That option is no longer on this panel."
pub const NO_OPTION: &str = "⚠️ That option is no longer on this panel.";
/// The member-facing error.
pub const OOPS: &str = "Something went wrong while updating your roles. Please try again.";
/// The rate-limit notice.
pub const RATE_MESSAGE: &str = "You're changing roles too quickly. Wait a few seconds and try again.";

fn member_key(guild_id: &str, user_id: &str) -> String {
    format!("{guild_id}:{user_id}")
}

/// `refreshMember(member)`: a forced REST read of the member's roles (the interaction's snapshot can be stale);
/// falls back to the snapshot.
async fn refresh_roles(app: &App, guild_id: &str, user_id: &str, snapshot: Vec<String>) -> Vec<String> {
    discord_ops(app).member_roles(guild_id, user_id).await.unwrap_or(snapshot)
}

fn member_roles_of(member: Option<&serenity::all::Member>) -> Vec<String> {
    member.map(|m| m.roles.iter().map(|r| r.to_string()).collect()).unwrap_or_default()
}

/// `handleRoleButton(interaction, client)`: `rr:{groupId}:{roleId}` (button panels, also the ids of panels posted
/// before the rewrite).
pub async fn handle_role_button(ctx: ComponentCtx) -> BotResult {
    let res: anyhow::Result<()> = async {
        let (_, params) = ctx.parsed();
        let group_id = params.first().cloned().unwrap_or_default();
        let role_id = params.get(1).cloned().unwrap_or_default();
        if !safe_defer(&ctx.responder, true).await {
            return Ok(());
        }
        let say = |content: String| {
            let r = ctx.responder.clone();
            async move {
                safe_reply(&r, Payload::text(content).no_mentions()).await;
            }
        };
        let (Some(guild_id), Some(member)) = (ctx.guild_id(), ctx.member()) else {
            say(OOPS.into()).await;
            return Ok(());
        };
        let g = guild_id.to_string();
        let group = if !group_id.is_empty() && !role_id.is_empty() { get_panel(ctx.app.db(), &g, &group_id).await? } else { None };
        let Some(group) = group else {
            say(NO_PANEL.into()).await;
            return Ok(());
        };
        let panel_role_ids = group.role_ids();
        if !panel_role_ids.contains(&role_id) {
            say(NO_OPTION.into()).await;
            return Ok(());
        }
        let user_id = ctx.user().id.to_string();
        let snapshot = member_roles_of(Some(member));
        let (app, gid, uid) = (ctx.app.clone(), g.clone(), user_id.clone());
        let kind = group.kind().to_string();
        let panel = PlanPanel { title: group.group.title.clone(), max_roles: group.group.max_roles };
        let content = rr_state(&ctx.app)
            .member_mutex
            .run(&member_key(&g, &user_id), move || async move {
                if over_rate_limit(&app, &gid, &uid).await {
                    return RATE_MESSAGE.to_string();
                }
                // A forced REST read inside the lock: single-id role calls do not update the cached member, so a
                // stale snapshot would report "Added" again on a fast second click.
                let roles = refresh_roles(&app, &gid, &uid, snapshot).await;
                let held: Vec<String> = panel_role_ids.iter().filter(|id| roles.contains(id)).cloned().collect();
                let target = vec![role_id];
                let plan = plan_role_change(&kind, Source::Button, &panel_role_ids, &held, &target, panel.max_roles);
                execute_plan(&app, &gid, &uid, &roles, &panel, &plan, &target).await
            })
            .await;
        say(content).await;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!("reaction-roles button failed", bot_core::error::log_arg(&e));
        safe_reply(&ctx.responder, Payload::text(OOPS).ephemeral(true).no_mentions()).await;
    }
    Ok(())
}

/// The select reply: `safeReply` on the shared panel, an edit of the personal message after `deferUpdate`.
async fn select_reply(ctx: &ComponentCtx, personal: bool, content: &str, rows: Vec<serenity::builder::CreateActionRow>) {
    if !personal {
        safe_reply(&ctx.responder, Payload::text(content).components(rows).no_mentions()).await;
        return;
    }
    // After deferUpdate, safeReply would send a follow-up; editing the personal message is what we want.
    let edit = EditInteractionResponse::new().content(content).components(rows).allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = ctx.edit_reply(edit).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!("reaction-roles personal menu update failed", e.to_string());
    }
}

/// The personal-menu footer line.
pub const PERSONAL_NOTE: &str = "\n-# Your current roles from this panel are selected below. Change them there.";
/// The personal menu's placeholder.
pub const PERSONAL_PLACEHOLDER: &str = "Adjust your roles from this panel";

/// `handleRoleSelect(interaction, client)`: `rr-select:{groupId}`, the shared dropdown on the panel and the personal
/// copy sent after each submission.
pub async fn handle_role_select(ctx: ComponentCtx) -> BotResult {
    let personal = ctx.interaction.message.flags.is_some_and(|f| f.contains(serenity::all::MessageFlags::EPHEMERAL));
    let res: anyhow::Result<()> = async {
        if personal {
            if let Err(e) = ctx.defer_update().await {
                if is_interaction_gone(&e) {
                    return Ok(());
                }
                return Err(e.into());
            }
        } else if !safe_defer(&ctx.responder, true).await {
            return Ok(());
        }
        let (_, params) = ctx.parsed();
        let group_id = params.first().cloned().unwrap_or_default();
        let (Some(guild_id), Some(member)) = (ctx.guild_id(), ctx.member()) else {
            select_reply(&ctx, personal, OOPS, Vec::new()).await;
            return Ok(());
        };
        let g = guild_id.to_string();
        let group = if group_id.is_empty() { None } else { get_panel(ctx.app.db(), &g, &group_id).await? };
        let Some(group) = group else {
            select_reply(&ctx, personal, NO_PANEL, Vec::new()).await;
            return Ok(());
        };
        let panel_role_ids = group.role_ids();
        let submitted = ctx.values();
        let values: Vec<String> = submitted.iter().filter(|v| panel_role_ids.contains(v)).cloned().collect();
        if !submitted.is_empty() && values.is_empty() {
            select_reply(&ctx, personal, NO_OPTION, Vec::new()).await;
            return Ok(());
        }
        let user_id = ctx.user().id.to_string();
        let snapshot = member_roles_of(Some(member));
        let (app, gid, uid) = (ctx.app.clone(), g.clone(), user_id.clone());
        let kind = group.kind().to_string();
        let panel = PlanPanel { title: group.group.title.clone(), max_roles: group.group.max_roles };
        let (pr, vals) = (panel_role_ids.clone(), values.clone());
        let out = rr_state(&ctx.app)
            .member_mutex
            .run(&member_key(&g, &user_id), move || async move {
                if over_rate_limit(&app, &gid, &uid).await {
                    return (RATE_MESSAGE.to_string(), None);
                }
                let roles = refresh_roles(&app, &gid, &uid, snapshot).await;
                let held: Vec<String> = pr.iter().filter(|id| roles.contains(id)).cloned().collect();
                let plan = plan_role_change(&kind, Source::Select, &pr, &held, &vals, panel.max_roles);
                let o = run_plan(&app, &gid, &uid, &roles, &panel, &plan, &vals).await;
                (o.content.clone(), Some((held, o)))
            })
            .await;

        let (mut content, detail) = out;
        let mut rows = Vec::new();
        if let Some((held, o)) = detail
            && (group.kind() == "NORMAL" || group.kind() == "UNIQUE")
        {
            let held_after: Vec<String> =
                panel_role_ids.iter().filter(|id| (held.contains(id) && !o.removed.contains(id)) || o.added.contains(id)).cloned().collect();
            if let Some(row) = build_select_row(&group.view(), Some(&held_after), Some(PERSONAL_PLACEHOLDER)) {
                rows.push(super::pure::Row::Select(row).to_builder());
                content.push_str(PERSONAL_NOTE);
            }
        }
        select_reply(&ctx, personal, &content, rows).await;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!("reaction-roles select failed", bot_core::error::log_arg(&e));
        select_reply(&ctx, personal, OOPS, Vec::new()).await;
    }
    Ok(())
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/// `reactionHookFilter(ctx)`: sync and cheap. Does this event concern an emoji-reaction panel (or might it, while the
/// index is unknown or stale)?
pub fn reaction_hook_filter(app: &App, guild_id: Option<&str>, message_id: Option<&str>) -> bool {
    let (Some(g), Some(m)) = (guild_id, message_id) else { return false };
    match peek_panel_index(app, g) {
        Some(p) if p.fresh => p.by_message_id.get(m).is_some_and(|p| p.layout == "REACTIONS"),
        _ => true, // run() loads the index
    }
}

/// `messageDeleteFilter(ctx)`.
pub fn message_delete_filter(app: &App, guild_id: Option<&str>, message_id: Option<&str>) -> bool {
    let (Some(g), Some(m)) = (guild_id, message_id) else { return false };
    match peek_panel_index(app, g) {
        Some(p) if p.fresh => p.by_message_id.contains_key(m),
        _ => true,
    }
}

/// The fields of a reaction event the hook body reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionInput {
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub user_id: String,
    /// `reaction.emoji.id ?? reaction.emoji.name`.
    pub emoji_key: String,
}

/// `processReaction(ctx, source)`: the body of the reaction hooks; resolves when the work is done. `get_member` is
/// `ctx.getMember()` (the member's role ids, `None` when the user left).
pub async fn process_reaction<F, Fut>(app: &App, input: ReactionInput, source: Source, get_member: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<Vec<String>>> + Send + 'static,
{
    let Some(index) = get_panel_index(app, &input.guild_id).await else { return }; // fail closed
    let Some(panel) = index.get(&input.message_id).filter(|p| p.layout == "REACTIONS").cloned() else { return };
    let key = normalize_emoji_key(&input.emoji_key);
    let Some(item) = panel.items.iter().find(|i| i.key.as_deref() == Some(key.as_str())).cloned() else { return }; // foreign reactions are left alone
    let (message_id, user_id, guild_id) = (input.message_id.clone(), input.user_id.clone(), input.guild_id.clone());
    if source == Source::Unreact && consume_reaction_suppression(app, &message_id, &user_id, &key) {
        return;
    }
    let a = app.clone();
    let channel_id = input.channel_id.clone();
    rr_state(app)
        .member_mutex
        .run(&member_key(&guild_id, &user_id), move || async move {
            let app = a;
            if over_rate_limit(&app, &guild_id, &user_id).await {
                bot_util::log_debug!(format!("reaction-roles: rate limit hit for {user_id} in {guild_id}"));
                return;
            }
            let Some(snapshot) = get_member().await else { return }; // the user left
            let roles = refresh_roles(&app, &guild_id, &user_id, snapshot).await; // fresh roles inside the lock
            let ops = discord_ops(&app);
            let state = rr_state(&app);
            let panel_role_ids: Vec<String> = panel.items.iter().map(|i| i.role_id.clone()).collect();
            let held: Vec<String> = panel_role_ids.iter().filter(|id| roles.contains(id)).cloned().collect();
            let max = panel.max_roles.map(|m| m.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
            let plan = plan_role_change(&panel.kind, source, &panel_role_ids, &held, std::slice::from_ref(&item.role_id), max);
            let fetch_msg = || async { ops.fetch_message(&guild_id, &channel_id, &message_id).await.ok() };

            if plan.reason == Some(PlanReason::Max) {
                // At the cap: take the reaction back (nothing is posted or DMed).
                suppress_reaction_removal(&app, &message_id, &user_id, &key);
                if ops.bot_has_manage_messages(&guild_id, &panel.channel_id) {
                    let msg = fetch_msg().await;
                    remove_member_reaction(&app, msg.as_ref(), &key, &user_id).await;
                } else if state.warn_throttle.allow(&format!("{}:manage-messages", panel.id)) {
                    bot_util::log_warn!(format!(
                        "reaction-roles: panel {} in {guild_id} needs Manage Messages to take back reactions over the role limit",
                        panel.id
                    ));
                }
                return;
            }
            if plan.reason.is_some() {
                return;
            }

            let all: Vec<String> = plan.to_add.iter().chain(plan.to_remove.iter()).cloned().collect();
            let v = verify_roles(ops.as_ref(), &guild_id, &all).await;
            for id in &v.gone {
                prune_in_background(&app, &guild_id, id);
            }
            let lost: Vec<&String> = v.gone.iter().chain(v.unverified.iter()).collect();
            if plan.to_add.iter().any(|id| lost.contains(&id)) {
                return;
            }
            let usable = Plan { to_remove: plan.to_remove.iter().filter(|id| !lost.contains(id)).cloned().collect(), ..plan.clone() };
            if !v.blocked.is_empty() {
                if state.warn_throttle.allow(&format!("{}:blocked", panel.id)) {
                    bot_util::log_warn!(format!(
                        "reaction-roles: cannot manage role(s) {} for panel {} in {guild_id} ({})",
                        v.blocked.join(","),
                        panel.id,
                        if v.no_manage_roles { "no Manage Roles" } else { "above my highest role" }
                    ));
                }
                return;
            }
            if usable.to_add.is_empty() && usable.to_remove.is_empty() {
                return;
            }

            let res = apply_role_change(ops.as_ref(), &guild_id, &user_id, &roles, &usable, &role_reason(&panel.title)).await;
            if let Some(f) = &res.failed {
                if f.code == Some(10011) {
                    if role_verdict(ops.as_ref(), &guild_id, &f.role_id).await == RoleVerdict::Gone {
                        prune_in_background(&app, &guild_id, &f.role_id);
                    }
                } else if state.warn_throttle.allow(&format!("{}:failed", panel.id)) {
                    bot_util::log_warn!(format!(
                        "reaction-roles: could not {} role {} for panel {} in {guild_id} (code {})",
                        f.op,
                        f.role_id,
                        panel.id,
                        f.code.map(|c| c.to_string()).unwrap_or_else(|| "null".into())
                    ));
                }
            }

            // UNIQUE: the roles that were swapped out also lose their reactions.
            if panel.kind == "UNIQUE" && source == Source::React && !res.removed.is_empty() {
                let mut msg = None;
                for role_id in &res.removed {
                    let Some(other_key) = panel.items.iter().find(|i| &i.role_id == role_id && i.key.is_some()).and_then(|i| i.key.clone()) else {
                        continue;
                    };
                    if !ops.bot_has_manage_messages(&guild_id, &panel.channel_id) {
                        if state.warn_throttle.allow(&format!("{}:manage-messages", panel.id)) {
                            bot_util::log_warn!(format!(
                                "reaction-roles: panel {} in {guild_id} needs Manage Messages to remove swapped-out reactions",
                                panel.id
                            ));
                        }
                        break;
                    }
                    suppress_reaction_removal(&app, &message_id, &user_id, &other_key);
                    if msg.is_none() {
                        msg = Some(fetch_msg().await);
                    }
                    remove_member_reaction(&app, msg.as_ref().and_then(|m| m.as_ref()), &other_key, &user_id).await;
                }
            }
        })
        .await;
}

fn reaction_input(e: &ReactionEvent) -> ReactionInput {
    ReactionInput {
        guild_id: e.guild_id.to_string(),
        channel_id: e.channel_id.to_string(),
        message_id: e.message_id.to_string(),
        user_id: e.user.id.to_string(),
        emoji_key: e.emoji_key.clone(),
    }
}

fn start_reaction_work(app: &App, event: ReactionEvent, source: Source) {
    let a = app.clone();
    run_in_background(app, "reaction", async move {
        let input = reaction_input(&event);
        let (app2, ev) = (a.clone(), event.clone());
        process_reaction(&a, input, source, move || async move {
            ev.get_member(&app2).await.map(|m| m.roles.iter().map(|r| r.to_string()).collect())
        })
        .await;
        Ok(())
    });
}

/// `onReactionAdd(ctx)`: returns at once so starboard, achievements and automations are never held up.
pub async fn on_reaction_add(ctx: Arc<HookCtx<MessageReactionAdd>>) -> BotResult {
    start_reaction_work(&ctx.app, ctx.data.0.clone(), Source::React);
    Ok(())
}

/// `onReactionRemove(ctx)`.
pub async fn on_reaction_remove(ctx: Arc<HookCtx<MessageReactionRemove>>) -> BotResult {
    start_reaction_work(&ctx.app, ctx.data.0.clone(), Source::Unreact);
    Ok(())
}

/// `onReactionRemoveAll(ctx)`'s work: after a moderator clears every reaction, the bot puts its own back (roles never
/// change).
pub async fn restore_all_reactions(app: &App, guild_id: &str, channel_id: &str, message_id: &str) {
    let Some(index) = get_panel_index(app, guild_id).await else { return };
    let Some(panel) = index.get(message_id).filter(|p| p.layout == "REACTIONS") else { return };
    if !rr_state(app).restore_throttle.allow(message_id) {
        return;
    }
    if let Ok(msg) = discord_ops(app).fetch_message(guild_id, channel_id, message_id).await {
        ensure_panel_reactions(app, &msg, &panel.items).await;
    }
}

/// `onReactionRemoveEmoji(ctx)`'s work: after one emoji is cleared from a panel message, the bot puts its own reaction
/// back when that emoji is an item.
pub async fn restore_emoji_reaction(app: &App, guild_id: &str, channel_id: &str, message_id: &str, emoji_key: &str) {
    let Some(index) = get_panel_index(app, guild_id).await else { return };
    let Some(panel) = index.get(message_id).filter(|p| p.layout == "REACTIONS") else { return };
    let key = normalize_emoji_key(emoji_key);
    let Some(item) = panel.items.iter().find(|i| i.key.as_deref() == Some(key.as_str())) else { return };
    let Some(emoji) = item.emoji.clone() else { return };
    if let Ok(msg) = discord_ops(app).fetch_message(guild_id, channel_id, message_id).await {
        add_panel_reaction(app, &msg, &emoji).await;
    }
}

/// `onReactionRemoveAll(ctx)`.
pub async fn on_reaction_remove_all(ctx: Arc<HookCtx<MessageReactionRemoveAll>>) -> BotResult {
    let (a, g, c, m) = (ctx.app.clone(), ctx.data.guild_id.to_string(), ctx.data.channel_id.to_string(), ctx.data.message_id.to_string());
    run_in_background(&ctx.app, "restore reactions", async move {
        restore_all_reactions(&a, &g, &c, &m).await;
        Ok(())
    });
    Ok(())
}

/// `onReactionRemoveEmoji(ctx)`.
pub async fn on_reaction_remove_emoji(ctx: Arc<HookCtx<MessageReactionRemoveEmoji>>) -> BotResult {
    let d = &ctx.data;
    let (a, g, c, m, k) = (ctx.app.clone(), d.guild_id.to_string(), d.channel_id.to_string(), d.message_id.to_string(), d.emoji_key.clone());
    run_in_background(&ctx.app, "restore reaction", async move {
        restore_emoji_reaction(&a, &g, &c, &m, &k).await;
        Ok(())
    });
    Ok(())
}

/// `onMessageDelete(ctx)`: a panel message was deleted; flag the panel so `/reactionrole list` can say so and repost
/// can restore it.
pub async fn handle_message_delete(app: &App, guild_id: &str, message_id: &str) {
    if bot_core::deletion_tracker::was_deleted_by_bot(app, message_id).is_some_and(|r| r.starts_with("reaction-roles:")) {
        return; // our own delete / repost
    }
    let Some(index) = get_panel_index(app, guild_id).await else { return };
    let Some(panel) = index.get(message_id) else { return };
    bot_util::log_warn!(format!(
        "reaction-roles: panel \"{}\" ({}) in guild {guild_id} lost its message {message_id}. An admin can run /reactionrole repost.",
        bot_util::text::clamp_text(&panel.title, 80),
        panel.id
    ));
    if !discord_ops(app).guild_available(guild_id) {
        return; // rule 20
    }
    if let Err(e) = flag_message_panels_missing(app.db(), guild_id, message_id).await {
        bot_util::log_error!(format!("reaction-roles: could not flag the panels of message {message_id}"), e.to_string());
    }
}

/// The `messageDelete` hook.
pub async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    handle_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await;
    Ok(())
}

/// `onRoleDelete(ctx)`: a role was deleted; drop its items and re-render the panels.
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    prune_items_for_role(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string()).await;
    Ok(())
}
