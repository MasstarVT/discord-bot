import { Events } from 'discord.js';
import { cache } from '../services/redis.js';
import logger from '../utils/logger.js';

export const name = Events.GuildDelete;
export const once = false;

export async function execute(guild, client) {
  // Unavailable guilds (Discord outage) should not trigger data removal
  if (!guild.available) return;

  try {
    // Purge all Redis keys scoped to this guild
    await cache.invalidate(`guild:${guild.id}:*`);
    await cache.invalidate(`xp:cooldown:${guild.id}:*`);

    // NOTE: We intentionally do NOT delete GuildSettings from Postgres here.
    // The data is preserved in case the bot is re-invited. A scheduled cleanup
    // job (Module G, future) can prune rows that have been gone for > 30 days.
    logger.info(`Left guild "${guild.name}" (${guild.id}) — Redis keys cleared.`);
  } catch (err) {
    logger.error(`guildDelete: cleanup failed for guild ${guild.id}`, err);
  }
}
