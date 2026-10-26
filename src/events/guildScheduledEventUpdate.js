import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildScheduledEventUpdate;
export const once = false;

// Scheduled events are not cached, so the old one is always null.
export async function execute(oldScheduledEvent, newScheduledEvent, client) {
  await runHooks('guildScheduledEventUpdate', {
    client, guild: newScheduledEvent.guild, oldScheduledEvent, scheduledEvent: newScheduledEvent,
  });
}
