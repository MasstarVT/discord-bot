// Moderation plugin (pkg-moderation.md §6): glue only. Temp-ban / timeout / mute expiry jobs, ban import and
// reconciliation on guildReady, the mute re-apply on rejoin, and the external ban / unban recorders.
// No side effects at import: the ban-sync queue below is an empty structure, not a timer.
import { isModuleEnabled } from '../config/modules.js';
import { cache } from '../services/redis.js';
import { createKeyedMutex } from '../utils/async.js';
import logger from '../utils/logger.js';
import {
  handleExternalUnban,
  reapplyMuteOnJoin,
  reconcileExpiries,
  recordExternalBan,
  runExpireJob,
  runUnbanJob,
  syncGuildBans,
} from '../services/moderationService.js';

const banSyncQueue = createKeyedMutex();
const BANSYNC_TTL = 21_600; // 6 hours

export const plugin = {
  id: 'moderation',
  module: 'moderation',
  hooks: [
    {
      event: 'guildReady',
      id: 'moderation',
      always: true,
      timeoutMs: 30_000,
      run: async (ctx) => {
        // ALWAYS: a stated duration must never be exceeded, whatever the module toggle says.
        await reconcileExpiries(ctx.guild);
        const settings = await ctx.getSettings();
        if (!isModuleEnabled(settings, 'moderation')) return; // the ban import only runs while the module is on
        const key = `mod:bansync:${ctx.guild.id}`;
        let run;
        if (ctx.reason === 'join') {
          run = true;
          await cache.set(key, '1', BANSYNC_TTL);
        } else {
          // 'startup' and 'available' behave alike. Redis down (null) fails OPEN: run the import.
          run = (await cache.setNX(key, '1', BANSYNC_TTL)) !== false;
        }
        if (!run) return;
        // Detached so it never blocks the later guildReady hooks; serialised across guilds.
        void banSyncQueue.run('bansync', () => syncGuildBans(ctx.guild, ctx.client.user.id))
          .catch((err) => logger.error(`Ban sync failed for ${ctx.guild.id}`, err));
      },
    },
    {
      // Gated by plugin.module; granted slot right after `invites` (mute evasion).
      event: 'guildMemberAdd',
      id: 'moderation',
      filter: (ctx) => !ctx.member?.user?.bot,
      run: (ctx) => reapplyMuteOnJoin(ctx),
    },
    {
      event: 'guildBanAdd',
      id: 'moderation',
      filter: (ctx) => !!ctx.user && ctx.user.id !== ctx.client.user.id,
      run: (ctx) => recordExternalBan(ctx),
    },
    {
      // `always`: rows are resolved and the temp-ban job cancelled even when the module is off.
      event: 'guildBanRemove',
      id: 'moderation',
      always: true,
      filter: (ctx) => !!ctx.user,
      run: async (ctx) => handleExternalUnban({
        client: ctx.client,
        guild: ctx.guild,
        user: ctx.user,
        settings: await ctx.getSettings(),
      }),
    },
  ],
  components: [],
  jobs: [
    { type: 'mod.unban', handler: (job, ctx) => runUnbanJob(job, ctx) },
    { type: 'mod.expire', handler: (job, ctx) => runExpireJob(job, ctx) },
  ],
  provides: {},
};
