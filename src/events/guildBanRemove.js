import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildBanRemove;
export const once = false;

export async function execute(ban, client) {
  await runHooks('guildBanRemove', { client, guild: ban.guild, ban, user: ban.user });
}
