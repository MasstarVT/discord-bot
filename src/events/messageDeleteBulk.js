import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageBulkDelete;
export const once = false;

export async function execute(messages, channel, client) {
  if (!channel.guild) return;
  await runHooks('messageDeleteBulk', { client, guild: channel.guild, messages, channel });
}
