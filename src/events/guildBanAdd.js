import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildBanAdd;
export const once = false;

export async function execute(ban, client) {
  await runHooks('guildBanAdd', { client, guild: ban.guild, ban, user: ban.user });
}
