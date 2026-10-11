import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ChannelUpdate;
export const once = false;

export async function execute(oldChannel, newChannel, client) {
  if (!newChannel.guild) return;
  await runHooks('channelUpdate', { client, guild: newChannel.guild, oldChannel, newChannel, channel: newChannel });
}
