import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ThreadDelete;
export const once = false;

export async function execute(thread, client) {
  if (!thread?.guild) return;
  await runHooks('threadDelete', { client, guild: thread.guild, thread, channel: thread });
}
