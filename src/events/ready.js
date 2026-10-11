import { Events, ActivityType } from 'discord.js';
import { runGuildReady, runHooks } from '../handlers/hookRegistry.js';
import { start as startScheduler } from '../services/scheduler.js';
import { mapLimit } from '../utils/async.js';
import logger from '../utils/logger.js';

export const name = Events.ClientReady;
export const once = true;

export async function execute(client) {
  logger.success(`Ready as ${client.user.tag} | ${client.guilds.cache.size} guild(s) | Shard ${client.shard?.ids?.[0] ?? 0}`);
  client.user.setActivity('your server', { type: ActivityType.Watching });

  startScheduler(client); // claims jobs of this shard's available guilds only
  await runHooks('ready', { client, guild: null });

  // Unavailable guilds (an outage, or still missing when waitGuildTimeout fired) have empty caches:
  // skip them here. guildAvailable.js runs their guildReady when they arrive.
  const available = [...client.guilds.cache.values()].filter((g) => g.available);
  await mapLimit(available, 4, (guild) => runGuildReady(client, guild, 'startup')); // R122
  logger.info(`guildReady completed for ${available.length}/${client.guilds.cache.size} guild(s).`);
}
