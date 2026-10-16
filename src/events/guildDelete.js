import { Events } from 'discord.js';
import { RECURRING_JOB_TYPES } from '../config/jobTypes.js';
import { runHooks } from '../handlers/hookRegistry.js';
import { cache } from '../services/redis.js';
import { cancelJobs } from '../services/scheduler.js';
import { invalidateSettings } from '../services/settingsService.js';
import logger from '../utils/logger.js';

export const name = Events.GuildDelete;
export const once = false;

export async function execute(guild, client) {
  // Unavailable guilds (a Discord outage) are not departures: keep everything.
  if (!guild.available) return;

  try {
    await invalidateSettings(guild.id);
    // Purge every Redis key scoped to this guild, in one pass over the keyspace.
    await cache.invalidateAny(`*:${guild.id}:*`, [`guild:${guild.id}:*`, `xp:cooldown:${guild.id}:*`, `voice:start:${guild.id}:*`]);
  } catch (err) {
    logger.error(`guildDelete: cache cleanup failed for guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
  }

  try {
    // Recurring jobs (stats, birthdays, feeds, reminders, voice sweeps) would stay PENDING forever;
    // the guildReady hooks re-seed them after a re-invite. One-off jobs are kept.
    await cancelJobs({ guildId: guild.id, types: RECURRING_JOB_TYPES });
  } catch (err) {
    logger.error(`guildDelete: could not cancel recurring jobs of guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
  }

  await runHooks('guildDelete', { client, guild });

  // Database rows are intentionally kept, so a re-invite restores everything.
  logger.info(`Left guild "${guild.name}" (${guild.id}) — cache cleared, recurring jobs cancelled.`);
}
