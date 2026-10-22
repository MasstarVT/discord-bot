// Plugin `backups`: server backups (roles, categories, channels and permission overwrites) and the restore that rebuilds what
// is missing. Declaration only: every body lives in src/services/backupService.js (restore engine: src/utils/guildSnapshot.js)
// and nothing runs at import.
import * as backups from '../services/backupService.js';

export const plugin = {
  id: 'backups',
  module: 'backups',
  hooks: [
    // Database only: keeps the recurring backup.auto job alive after a restart (the job reads the settings itself, so the
    // module being off at that moment does not matter). `always` because the module may be turned on later.
    { event: 'guildReady', id: 'backups', run: backups.onGuildReady, always: true },
  ],
  components: [
    // module: null: the confirmation works while the module is off. The handler checks for the SERVER OWNER itself.
    { type: 'button', prefix: 'backup', module: null, handler: (interaction, client) => backups.handleBackupButton(interaction, client) },
  ],
  jobs: [
    // Not gated by the runner: the handlers read the module toggle themselves, and a running restore always finishes.
    { type: 'backup.auto', handler: (job, ctx) => backups.handleAutoJob(job, ctx) },
    { type: 'backup.restore', handler: (job, ctx) => backups.handleRestoreJob(job, ctx) },
  ],
  provides: {},
};
