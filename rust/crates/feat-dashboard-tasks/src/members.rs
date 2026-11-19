//! Port of `src/services/dashboardTasks/members.js`: member XP changes the web dashboard asks the bot to do (its
//! `levels` list):
//!   `xp.adjust`   `{ op: 'give'|'take'|'set'|'reset', userId, amount? }`: /levelconfig xp give (one member) | remove |
//!   set | reset
//! The same xpService calls as /levelconfig xp: one atomic SQL statement per change (clamped to XP_MAX), then
//! handleLevelChange (the stored level, reward roles, the `levelUp` hook; announce off), and for a reset the role sync
//! on demotion. Like /levelconfig xp (gated = false, ADMIN), it works while the Levels module is off.
//! Coins need no task: the dashboard's `balances` store calls economyService directly.

use bot_config::{PermissionLevel, XP_MAX};
use bot_core::App;
use feat_levels::xp_service::{self, LevelChange, RoleSync, SyncOptions, XpChange, XpDelta};
use serde_json::{Value, json};
use serenity::all::{Member, User};

use crate::index::{DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util::{self, fmt};

pub const XP_OPS: [&str; 4] = ["give", "take", "set", "reset"];
/// /levelconfig xp give and /levelconfig xp remove: 1 to 1,000,000; /levelconfig xp set: 0 to XP_MAX.
pub const XP_STEP_MAX: i64 = 1_000_000;

/// A re-validated `xp.adjust` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XpPayload {
    pub op: String,
    pub user_id: String,
    /// `None` for a reset.
    pub amount: Option<i64>,
}

/// `readXpPayload(payload)`.
pub fn read_xp_payload(payload: &Value) -> Result<XpPayload, TaskError> {
    if !util::is_object(payload) {
        return Err(TaskError::new("The request was malformed."));
    }
    let Some(op) = payload.get("op").and_then(Value::as_str).filter(|op| XP_OPS.contains(op)) else {
        return Err(TaskError::new("Unknown XP change."));
    };
    let user_id = util::js_string_or_empty(payload.get("userId"));
    if !util::is_snowflake(&user_id) {
        return Err(TaskError::new("Pick a member, or paste a user ID."));
    }
    if op == "reset" {
        return Ok(XpPayload { op: op.into(), user_id, amount: None });
    }
    let (min, max) = if op == "set" { (0.0, f64::from(XP_MAX)) } else { (1.0, XP_STEP_MAX as f64) };
    match util::js_integer(payload.get("amount")) {
        Some(a) if a >= min && a <= max => Ok(XpPayload { op: op.into(), user_id, amount: Some(a as i64) }),
        _ => Err(TaskError::new(format!("The amount must be between {} and {} XP.", fmt(min), fmt(max)))),
    }
}

/// The member (None when not in the server) and the user (None when unknown to Discord).
pub struct Who {
    pub member: Option<Member>,
    pub user: Option<User>,
}

async fn lookup(app: &App, guild_id: &str, user_id: &str) -> Who {
    let member = util::fetch_member(app, guild_id, user_id).await;
    let user = match &member {
        Some(m) => Some(m.user.clone()),
        None => util::fetch_user(app, user_id).await,
    };
    Who { member, user }
}

/// `nameOf({ member, user }, userId)`.
pub fn name_of(who: &Who, user_id: &str) -> String {
    if let Some(m) = &who.member {
        let n = util::member_display_name(m);
        if !n.is_empty() {
            return n;
        }
    }
    who.user.as_ref().map(util::user_name).filter(|n| !n.is_empty()).unwrap_or_else(|| user_id.to_string())
}

/// `roleName(guild, id)`: `@name`, or the id when the role is not cached.
pub fn role_name(app: &App, guild_id: &str, id: &str) -> String {
    util::cached_role_name(app, guild_id, id).map(|n| format!("@{n}")).unwrap_or_else(|| id.to_string())
}

/// `changeText(guild, result, lc)`: the level change and reward-role lines of an admin change (without mentions).
pub fn change_text(old_level: i64, new_level: i64, roles: Option<&RoleSync>, role_name: &dyn Fn(&str) -> String) -> String {
    let mut lines = Vec::new();
    if new_level != old_level {
        lines.push(format!("Level {old_level} → {new_level}."));
    }
    if let Some(roles) = roles {
        let names = |ids: &mut dyn Iterator<Item = &String>| ids.map(|id| role_name(id)).collect::<Vec<_>>().join(", ");
        if !roles.added.is_empty() {
            lines.push(format!("Roles added: {}.", names(&mut roles.added.iter())));
        }
        if !roles.removed.is_empty() {
            lines.push(format!("Roles removed: {}.", names(&mut roles.removed.iter())));
        }
        if !roles.failed.is_empty() {
            lines.push(format!(
                "Could not change: {} (check the bot's role position and Manage Roles).",
                names(&mut roles.failed.iter().map(|(id, _)| id))
            ));
        }
    }
    if lines.is_empty() { String::new() } else { format!(" {}", lines.join(" ")) }
}

/// The message of a `give`.
pub fn give_message(amount: i64, name: &str, r: &XpChange, change: &str) -> String {
    let mut message = format!("Gave {} XP to {name}. Total: {} XP (level {}).{change}", fmt(amount as f64), fmt(r.xp as f64), r.new_level);
    if r.xp == i64::from(XP_MAX) {
        message.push_str(&format!(" XP is capped at {}.", fmt(f64::from(XP_MAX))));
    }
    message
}

/// The message of a `take`.
pub fn take_message(name: &str, r: &XpChange, change: &str) -> String {
    format!("Removed {} XP from {name}. Total: {} XP (level {}).{change}", fmt((r.old_xp.unwrap_or(0) - r.xp) as f64), fmt(r.xp as f64), r.new_level)
}

/// The message of a `set`.
pub fn set_message(name: &str, r: &XpChange, change: &str) -> String {
    format!("{name} now has {} XP (level {}).{change}", fmt(r.xp as f64), r.new_level)
}

/// The message of a `reset`.
pub fn reset_message(name: &str, old_level: i64, removed: Option<&[String]>, role_name: &dyn Fn(&str) -> String) -> String {
    let removed = match removed {
        Some(ids) if !ids.is_empty() => format!(" Roles removed: {}.", ids.iter().map(|id| role_name(id)).collect::<Vec<_>>().join(", ")),
        _ => String::new(),
    };
    format!("Wiped XP and level for {name} (was level {old_level}).{removed}")
}

/// `xp.adjust` -> `{ userId, xp, level }` after the change.
pub async fn adjust_xp(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let XpPayload { op, user_id, amount } = read_xp_payload(&ctx.payload)?;
    let actor = &ctx.actor;
    if actor.member.is_none() && !actor.is_bot_owner {
        return Err(task_error("You are no longer a member of this server."));
    }
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
        return Err(task_error("The bot couldn't load this server's settings. Try again in a moment."));
    };
    // /levelconfig xp is an ADMIN command; the bot owner (OWNER level) passes even without being a member.
    if !actor.is_bot_owner {
        let level = actor.member.as_ref().map(|m| util::member_level(app, guild_id, m, Some(&settings))).unwrap_or(PermissionLevel::Member);
        if level < PermissionLevel::Admin {
            return Err(task_error("Only administrators (Manage Server) can change XP."));
        }
    }
    let Some(g) = util::gid(guild_id) else { return Err(task_error("Pick a member, or paste a user ID.")) };
    let who = lookup(app, guild_id, &user_id).await;
    let name = name_of(&who, &user_id);
    let rn = |id: &str| role_name(app, guild_id, id);
    let change = |r: XpChange| {
        let c = LevelChange::of(g, &user_id, &r, Some(settings.clone()), "admin").member(who.member.clone()).channel(None).announce(false);
        xp_service::handle_level_change(app, c)
    };
    let is_bot = who.user.as_ref().is_some_and(|u| u.bot);
    let data = |r: &XpChange| json!({ "userId": user_id, "xp": r.xp, "level": r.new_level });

    match op.as_str() {
        "give" => {
            let amount = amount.unwrap_or(1);
            if is_bot {
                return Err(task_error("Bots don't earn XP."));
            }
            if who.member.is_none() {
                return Err(task_error("That user isn't in this server."));
            }
            let r = xp_service::apply_xp_delta(app, XpDelta::new(guild_id, &user_id, amount as f64)).await?;
            let lc = change(r).await;
            let text = change_text(r.old_level, r.new_level, lc.roles.as_ref(), &rn);
            Ok(TaskOutcome::new(give_message(amount, &name, &r, &text), data(&r)))
        }
        "take" => {
            let amount = amount.unwrap_or(1);
            let r = xp_service::subtract_xp(app, guild_id, &user_id, amount as f64).await?;
            let Some(r) = r.filter(|r| r.old_xp != Some(0)) else { return Err(task_error(format!("{name} has no XP in this server."))) };
            let lc = change(r).await;
            let text = change_text(r.old_level, r.new_level, lc.roles.as_ref(), &rn);
            Ok(TaskOutcome::new(take_message(&name, &r, &text), data(&r)))
        }
        "set" => {
            let amount = amount.unwrap_or(0);
            if amount > 0 {
                if is_bot {
                    return Err(task_error("Bots don't earn XP."));
                }
                if who.member.is_none() {
                    return Err(task_error("That user isn't in this server."));
                }
            }
            let Some(r) = xp_service::set_xp(app, guild_id, &user_id, amount as f64).await? else {
                return Err(task_error(format!("{name} already has 0 XP.")));
            };
            let lc = change(r).await;
            let text = change_text(r.old_level, r.new_level, lc.roles.as_ref(), &rn);
            Ok(TaskOutcome::new(set_message(&name, &r, &text), data(&r)))
        }
        _ => {
            let Some(old_level) = xp_service::reset_xp(app.db(), guild_id, &user_id).await? else {
                return Err(task_error(format!("{name} has no XP in this server.")));
            };
            let mut removed: Option<Vec<String>> = None;
            if settings.level_roles_sync_on_demotion
                && let Some(member) = &who.member
            {
                let opts = SyncOptions { sync_down: true, rewards: None, reason: "XP reset".into() };
                let roles = xp_service::sync_level_roles(app, g, member, 0, Some(&settings), opts).await;
                removed = Some(roles.removed);
            }
            Ok(TaskOutcome::new(reset_message(&name, old_level, removed.as_deref(), &rn), json!({ "userId": user_id, "xp": 0, "level": 0 })))
        }
    }
}

/// `createMemberTasks()`.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![("xp.adjust", DashboardTask::new(adjust_xp))]
}
