// Bump Reminders plugin declaration: the botMessage hook that sees a bump site's "bump done" answer, the cleanup hooks (a deleted
// reminder channel or role) and the `bump.remind` job. Declaration only: the rules live in services/bumpService.js and nothing
// runs at import.
import {
  channelDeleteFilter, handleRemindJob, isBumpSiteMessage, onBotMessage, onChannelDelete, onRoleDelete, roleDeleteFilter,
} from '../services/bumpService.js';

export const plugin = {
  id: 'bumps',
  module: 'bumps',
  hooks: [
    // Every bot message in every server reaches botMessage, so the sync filter only lets a known bump site's bot through
    // (before any settings lookup). Gated by the plugin's module: nothing is recorded while the module is off.
    { event: 'botMessage', id: 'bumps', filter: isBumpSiteMessage, run: (ctx) => onBotMessage(ctx) },
    // `always`: unsetting a dead channel or role is cleanup and must happen while the module is off as well.
    { event: 'channelDelete', id: 'bumps', always: true, filter: channelDeleteFilter, run: (ctx) => onChannelDelete(ctx) },
    { event: 'roleDelete', id: 'bumps', always: true, filter: roleDeleteFilter, run: (ctx) => onRoleDelete(ctx) },
  ],
  components: [],
  // Not module-gated by the scheduler: the handler checks the module itself, so a job due while the module is off just ends.
  jobs: [{ type: 'bump.remind', handler: (job, ctx) => handleRemindJob(job, ctx) }],
  provides: {},
};
