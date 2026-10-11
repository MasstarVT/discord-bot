// Invite Tracker plugin (pkg-invites). Attribution no longer depends on the welcome module: this plugin owns the
// join/leave bookkeeping (GuildMemberCache) and publishes the result to later guildMemberAdd hooks as
// ctx.state.invite = { code, inviterId, vanity } | null (null = unknown). Thin wrappers over inviteTracker.js.
import { isModuleEnabled } from '../services/settingsService.js';
import { withTimeout } from '../utils/async.js';
import {
  trackJoin, trackLeave, refreshSnapshot, recordInviteCreate, recordInviteDelete, JOIN_HOOK_WAIT_MS,
} from '../services/inviteTracker.js';

export const plugin = {
  id: 'invites',
  module: 'invites',
  hooks: [
    // 1st in guildReady: warm a TRUSTED snapshot before the other per-guild startup work (R122).
    { event: 'guildReady', id: 'invites', timeoutMs: 15_000,
      run: async (ctx) => { await refreshSnapshot(ctx.guild); } },

    // 1st in guildMemberAdd; always:true so join rows (joinedAt, leftAt = null) stay correct while the module is
    // off, and so NOTHING is awaited before trackJoin enqueues (ordering: the per-guild queue order must equal
    // the gateway order, 01-foundation §8.3).
    { event: 'guildMemberAdd', id: 'invites', always: true, timeoutMs: 10_000,
      run: async (ctx) => {
        ctx.state.invite = null;
        if (ctx.member?.user?.bot) return;                          // D8: bots never use an invite
        const p = trackJoin(ctx.member, {                            // MUST be the first call: it enqueues synchronously
          isEnabled: async () => isModuleEnabled(await ctx.getSettings(), 'invites'),
        });
        // Stop waiting after JOIN_HOOK_WAIT_MS: later hooks then see null while the queued work still writes the row.
        ctx.state.invite = await withTimeout(p, JOIN_HOOK_WAIT_MS, 'invite attribution').catch(() => null);
      } },

    // 1st in guildMemberRemove; always:true: leftAt must be recorded even while the module is off.
    { event: 'guildMemberRemove', id: 'invites', always: true,
      filter: (ctx) => (ctx.user ?? ctx.member?.user)?.bot !== true,
      run: (ctx) => { trackLeave(ctx.guild, ctx.user?.id ?? ctx.member?.id); } },   // fire-and-forget; never rejects

    // Incremental snapshot edits (no REST). Gated by the module.
    { event: 'inviteCreate', id: 'invites', run: (ctx) => recordInviteCreate(ctx.guild, ctx.invite) },
    { event: 'inviteDelete', id: 'invites', run: (ctx) => recordInviteDelete(ctx.guild, ctx.invite) },
  ],
  components: [],
  jobs: [],
  provides: {},
};
