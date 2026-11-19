//! Port of `src/plugins/dashboard-tasks.js`: runs the `dashboard.task` jobs the web dashboard queues for actions that
//! need the gateway client (moderation actions, XP and coin changes with their role syncs, posting giveaways, polls
//! and panels). Declaration only; the work is in [`crate::index`].

use bot_config::JobType;
use bot_core::Plugin;
use bot_core::scheduler::job_handler;

/// The `dashboard-tasks` plugin (core: `module: null`).
pub fn plugin() -> Plugin {
    Plugin::new("dashboard-tasks", None).job(JobType::DashboardTask, job_handler(crate::index::run_dashboard_task))
}
