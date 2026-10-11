import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.ChannelCreate;
export const once = false;

export async function execute(channel, client) {
  if (!channel.guild) return;
  await runHooks('channelCreate', { client, guild: channel.guild, channel });
}
