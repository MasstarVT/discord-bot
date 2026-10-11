import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ThreadCreate;
export const once = false;

export async function execute(thread, newlyCreated, client) {
  if (!newlyCreated || !thread.guild) return;
  await runHooks('threadCreate', { client, guild: thread.guild, thread, channel: thread });
}
