import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildScheduledEventDelete;
export const once = false;

// Scheduled events are not cached, so this is a partial (id and guild only).
export async function execute(scheduledEvent, client) {
  await runHooks('guildScheduledEventDelete', { client, guild: scheduledEvent.guild, scheduledEvent });
}
