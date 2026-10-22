// Server Boosts plugin declaration. Declaration only: every body lives in src/services/boostService.js and nothing runs at import.
// A thank-you when a member starts boosting, and an optional role of their own for boosters (/boost role).
import * as boosts from '../services/boostService.js';

export const plugin = {
  id: 'boosts',
  module: 'boosts',
  hooks: [
    // Gated by the module. The sync filter lets only a change of the boost state of a cached member through, so ordinary
    // member updates (roles, nicknames) cost nothing; a partial old member is never diffed.
    { event: 'guildMemberUpdate', id: 'boosts', filter: boosts.memberUpdateFilter, run: boosts.onMemberUpdate },
    { event: 'guildMemberRemove', id: 'boosts', filter: boosts.memberRemoveFilter, run: boosts.onMemberRemove },
    // Database only: a guild with booster roles gets its sweep job back after a restart or a re-invite.
    { event: 'guildReady', id: 'boosts', run: boosts.onGuildReady },
    // Cleanup runs even while the module is off, so a deleted channel or role never lingers in the settings or the table.
    { event: 'channelDelete', id: 'boosts', always: true, filter: boosts.channelDeleteFilter, run: boosts.onChannelDelete },
    { event: 'roleDelete', id: 'boosts', always: true, run: boosts.onRoleDelete },
  ],
  components: [], // no buttons, selects or modals
  // The handler cleans up whether or not the module is on, and ends itself when the guild has no booster roles left.
  jobs: [{ type: 'boosts.sweep', handler: boosts.sweepJobHandler }],
  provides: {},
};
