import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageReactionRemoveEmoji;
export const once = false;

// Caches the first promise, so every hook of one dispatch shares a single fetch.
const memo = (fn) => {
  let promise;
  return () => (promise ??= fn());
};

export async function execute(reaction, client) {
  if (!reaction.message.guildId) return;
  const guild = client.guilds.cache.get(reaction.message.guildId);
  if (!guild) return;

  await runHooks('messageReactionRemoveEmoji', {
    client,
    guild,
    reaction,
    message: reaction.message,
    emojiKey: reaction.emoji.id ?? reaction.emoji.name,
    ensureMessage: memo(async () => (reaction.message.partial ? reaction.message.fetch().catch(() => null) : reaction.message)),
  });
}
