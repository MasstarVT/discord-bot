// Dashboard tasks plugin: runs the `dashboard.task` jobs the web dashboard queues for actions that need the gateway
// client (moderation actions, XP and coin changes with their role syncs, posting giveaways, polls and panels).
// Declaration only: no logic and no side effects at import; the work is in src/services/dashboardTasks/.
import { runDashboardTask } from '../services/dashboardTasks/index.js';

export const plugin = {
  id: 'dashboard-tasks',
  module: null,
  hooks: [],
  components: [],
  jobs: [{ type: 'dashboard.task', handler: (job, ctx) => runDashboardTask(job, ctx) }],
  provides: {},
};
