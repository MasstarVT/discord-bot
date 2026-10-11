import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';
import { mapLimit } from '../utils/async.js';

export const name = Events.UserUpdate;
export const once = false;

// A user change (username, global name, avatar) is not guild-scoped: fan it out to every available
// guild that has the user cached, so each guild's hooks see it with that guild's member.
export async function execute(oldUser, newUser, client) {
  if (newUser.bot || oldUser.partial) return;

  const guilds = [...client.guilds.cache.values()].filter((g) => g.available && g.members.cache.has(newUser.id));
  await mapLimit(guilds, 4, (guild) => runHooks('userUpdate', {
    client,
    guild,
    oldUser,
    newUser,
    user: newUser,
    member: guild.members.cache.get(newUser.id),
  }));
}
