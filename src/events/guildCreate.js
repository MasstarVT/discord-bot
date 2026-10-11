import { Events } from 'discord.js';
import { runGuildReady } from '../handlers/hookRegistry.js';
import { invalidateSettings } from '../services/settingsService.js';
import logger from '../utils/logger.js';

export const name = Events.GuildCreate;
export const once = false;

export async function execute(guild, client) {
  try {
    // Bust any stale entry from a previous membership.
    await invalidateSettings(guild.id);
    // Ensures the settings row, then runs the guildReady hooks (invite snapshot, ban import, ...).
    await runGuildReady(client, guild, 'join');
    logger.info(`Joined guild "${guild.name}" (${guild.id}).`);
  } catch (err) {
    logger.error(`guildCreate: failed to set up guild ${guild.id}`, err instanceof Error ? err : new Error(String(err)));
  }
}
