import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageDelete;
export const once = false;

// Bot-authored messages are deliberately not filtered: giveaway, poll, starboard and panel
// messages are all authored by the bot, and their packages clean up when they are deleted.
export async function execute(message, client) {
  if (!message.guildId) return;
  const guild = message.guild ?? client.guilds.cache.get(message.guildId);
  if (!guild) return;

  await runHooks('messageDelete', {
    client,
    guild,
    message,
    channel: message.channel,
    partial: message.partial,
  });
}
