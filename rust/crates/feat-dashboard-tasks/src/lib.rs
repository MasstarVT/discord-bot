//! feat-dashboard-tasks: the `dashboard.task` jobs the web dashboard queues for actions that need the gateway client
//! (moderation actions, XP changes with their role syncs, posting giveaways, polls and panels, channel checks).
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/dashboardTasks/shared.js` | [`shared`] |
//! | `src/services/dashboardTasks/index.js` | [`index`] |
//! | `src/services/dashboardTasks/channelAccess.js` | [`channel_access`] |
//! | `src/services/dashboardTasks/members.js` | [`members`] |
//! | `src/services/dashboardTasks/moderation.js` | [`moderation`] |
//! | `src/services/dashboardTasks/giveaways.js` | [`giveaways`] |
//! | `src/services/dashboardTasks/polls.js` | [`polls`] |
//! | `src/services/dashboardTasks/reactionRoles.js` | [`reaction_roles`] |
//! | `src/services/dashboardTasks/tickets.js` | [`tickets`] |
//! | `src/plugins/dashboard-tasks.js` | [`plugin`] |
//!
//! The dashboard side of the protocol (`src/dashboard/tasks.js`) queues a job with [`shared::TASK_JOB_TYPE`],
//! [`shared::task_dedupe_key`] and the payload `{ kind, actorId, payload }`, then reads [`shared::task_result_key`]
//! (or the FAILED row's `lastError`).

pub mod channel_access;
pub mod giveaways;
pub mod index;
pub mod members;
pub mod moderation;
pub mod plugin;
pub mod polls;
pub mod reaction_roles;
pub mod shared;
pub mod tickets;
pub mod util;

/// Registers the `dashboard-tasks` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.plugin(plugin::plugin());
}
