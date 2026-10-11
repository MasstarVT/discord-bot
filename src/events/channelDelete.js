import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ChannelDelete;
export const once = false;

export async function execute(channel, client) {
  if (!channel.guild) return;
  await runHooks('channelDelete', { client, guild: channel.guild, channel });
}
