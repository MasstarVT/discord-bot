import { Events, ActivityType } from 'discord.js';
import prisma from '../database/client.js';
import { cacheGuildInvites } from '../services/inviteTracker.js';
import { syncGuildBans } from '../services/moderationService.js';
import logger from '../utils/logger.js';

export const name = Events.ClientReady;
export const once = true;

export async function execute(client) {
  logger.success(`Ready as ${client.user.tag} | ${client.guilds.cache.size} guild(s) | Shard ${client.shard?.ids[0] ?? 0}`);
  client.user.setActivity('your server', { type: ActivityType.Watching });

  // Ensure every guild has a GuildSettings row and import any pre-existing bans.
  for (const [, guild] of client.guilds.cache) {
    await prisma.guildSettings.upsert({
      where:  { guildId: guild.id },
      update: {},
      create: { guildId: guild.id },
    }).catch((err) => logger.error(`Failed to upsert GuildSettings for ${guild.id}`, err));

    await syncGuildBans(guild, client.user.id).catch((err) =>
      logger.error(`Failed to sync bans for guild ${guild.id}`, err)
    );
  }

  // Pre-warm invite cache for all guilds so the first join after startup
  // can still resolve which invite was used.
  let cached = 0;
  for (const [, guild] of client.guilds.cache) {
    await cacheGuildInvites(guild);
    cached++;
  }
  if (cached > 0) logger.info(`Invite cache warmed for ${cached} guild(s).`);
}
