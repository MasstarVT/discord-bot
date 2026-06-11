import { Events } from 'discord.js';
import prisma from '../database/client.js';
import { cache } from '../services/redis.js';
import { cacheGuildInvites } from '../services/inviteTracker.js';
import { syncGuildBans } from '../services/moderationService.js';
import logger from '../utils/logger.js';

export const name = Events.GuildCreate;
export const once = false;

export async function execute(guild, client) {
  try {
    await prisma.guildSettings.upsert({
      where:  { guildId: guild.id },
      update: {},
      create: { guildId: guild.id },
    });

    // Bust any stale cache entry from a previous session
    await cache.del(`guild:${guild.id}:settings`);

    await cacheGuildInvites(guild);

    // Import any bans that existed before the bot joined
    await syncGuildBans(guild, client.user.id);

    logger.info(`Joined guild "${guild.name}" (${guild.id}) — GuildSettings upserted.`);
  } catch (err) {
    logger.error(`guildCreate: failed to upsert settings for guild ${guild.id}`, err);
  }
}
