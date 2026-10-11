import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ThreadUpdate;
export const once = false;

export async function execute(oldThread, newThread, client) {
  if (!newThread?.guild) return;
  await runHooks('threadUpdate', { client, guild: newThread.guild, oldThread, newThread, thread: newThread, channel: newThread });
}
