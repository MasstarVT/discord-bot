import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildRoleCreate;
export const once = false;

export async function execute(role, client) {
  await runHooks('roleCreate', { client, guild: role.guild, role });
}
