import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.PresenceUpdate;
export const once = false;

// Presence updates are the highest-volume gateway event: no settings access and no I/O here.
// Hooks gate themselves with a sync `filter` (peekSettings) before doing any work.
export async function execute(oldPresence, newPresence, client) {
  const guild = newPresence?.guild;
  if (!guild) return;
  const member = newPresence.member;
  if (!member || member.user?.bot) return;

  await runHooks('presenceUpdate', { client, guild, oldPresence, newPresence, member });
}
