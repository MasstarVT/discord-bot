import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

// No pre-filter and nothing awaited before runHooks (gateway order, 01-foundation §8.3). The member may be partial.
export const name = Events.GuildMemberRemove;
export const once = false;

export async function execute(member, client) {
  await runHooks('guildMemberRemove', { client, guild: member.guild, member, user: member.user });
}
