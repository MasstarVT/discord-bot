import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

// No pre-filter and nothing awaited before runHooks: bots are passed through (hooks decide), and the hook runner relies
// on being invoked in gateway order (01-foundation §8.3, ordering guarantee).
export const name = Events.GuildMemberAdd;
export const once = false;

export async function execute(member, client) {
  await runHooks('guildMemberAdd', { client, guild: member.guild, member, user: member.user });
}
