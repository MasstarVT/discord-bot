import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

// oldKnown is false when oldMember was not cached (partial): hooks must not diff roles or nicknames then.
export const name = Events.GuildMemberUpdate;
export const once = false;

export async function execute(oldMember, newMember, client) {
  await runHooks('guildMemberUpdate', { client, guild: newMember.guild, oldMember, newMember, member: newMember, oldKnown: !oldMember.partial });
}
