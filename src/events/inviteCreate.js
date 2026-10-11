import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.InviteCreate;
export const once = false;

export async function execute(invite, client) {
  const guild = invite.guild ? client.guilds.cache.get(invite.guild.id) : null;
  if (!guild) return;

  await runHooks('inviteCreate', { client, guild, invite });
}
