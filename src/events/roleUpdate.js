import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildRoleUpdate;
export const once = false;

export async function execute(oldRole, newRole, client) {
  await runHooks('roleUpdate', { client, guild: newRole.guild, oldRole, newRole, role: newRole });
}
