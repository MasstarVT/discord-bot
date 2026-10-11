import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildUpdate;
export const once = false;

export async function execute(oldGuild, newGuild, client) {
  await runHooks('guildUpdate', { client, guild: newGuild, oldGuild, newGuild });
}
