import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageReactionRemoveAll;
export const once = false;

// Caches the first promise, so every hook of one dispatch shares a single fetch.
const memo = (fn) => {
  let promise;
  return () => (promise ??= fn());
};

export async function execute(message, reactions, client) {
  if (!message.guildId) return;
  const guild = client.guilds.cache.get(message.guildId);
  if (!guild) return;

  await runHooks('messageReactionRemoveAll', {
    client,
    guild,
    message,
    reactions,
    ensureMessage: memo(async () => (message.partial ? message.fetch().catch(() => null) : message)),
  });
}
