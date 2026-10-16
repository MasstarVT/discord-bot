import { Events } from 'discord.js';
import { runGuildAvailable } from '../handlers/hookRegistry.js';
import logger from '../utils/logger.js';

export const name = Events.GuildAvailable;
export const once = false;

// discord.js emits this (not guildCreate) when a guild that was unavailable at startup, or that went
// down in an outage, arrives. Its caches are populated now, so its startup work runs (capped, and skipped
// when it already ran in the last 10 minutes: see runGuildAvailable).
export async function execute(guild, client) {
  try {
    const ran = await runGuildAvailable(client, guild);
    logger.info(`Guild became available: ${guild.name} (${guild.id})${ran ? '' : '; startup work ran recently, skipped'}`);
  } catch (err) {
    logger.error(`guildAvailable: startup work failed for guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
  }
}
