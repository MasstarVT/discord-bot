import { Events } from 'discord.js';
import { runGuildReady } from '../handlers/hookRegistry.js';
import logger from '../utils/logger.js';

export const name = Events.GuildAvailable;
export const once = false;

// discord.js emits this (not guildCreate) when a guild that was unavailable at startup, or that went
// down in an outage, arrives. Its caches are populated now, so its startup work runs.
export async function execute(guild, client) {
  try {
    await runGuildReady(client, guild, 'available');
    logger.info(`Guild became available: ${guild.name} (${guild.id})`);
  } catch (err) {
    logger.error(`guildAvailable: startup work failed for guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
  }
}
