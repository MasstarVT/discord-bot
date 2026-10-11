import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildRoleDelete;
export const once = false;

export async function execute(role, client) {
  await runHooks('roleDelete', { client, guild: role.guild, role });
}
