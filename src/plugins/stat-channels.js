// Statistics channels plugin declaration (pkg-stat-channels §5). Declaration only: every body lives in services/statsService.js and
// nothing runs at import.
import * as stats from '../services/statsService.js';

export const plugin = {
  id: 'stat-channels',
  module: 'stats',
  hooks: [
    // Database only: primes the memo and keeps the recurring job alive, so turning the module back on works without a restart.
    { event: 'guildReady', id: 'stat-channels', run: stats.onGuildReady, always: true },
    // Gated by the `stats` module through the runner. The sync filters keep guilds without member-driven counters free of database work.
    { event: 'guildMemberAdd', id: 'stat-channels', filter: stats.memberEventFilter, run: stats.onMemberChange },
    { event: 'guildMemberRemove', id: 'stat-channels', filter: stats.memberEventFilter, run: stats.onMemberChange },
    // Cleanup must happen even while the module is off.
    { event: 'channelDelete', id: 'stat-channels', filter: stats.channelDeleteFilter, run: stats.onChannelDelete, always: true },
  ],
  components: [], // no buttons, selects or modals
  // The handler reads the module toggle itself: while the module is off it only reschedules, so turning it back on needs no restart.
  jobs: [{ type: 'stats.refresh', handler: stats.refreshJobHandler }],
  provides: {},
};
