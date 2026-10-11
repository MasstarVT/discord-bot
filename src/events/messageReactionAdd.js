import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageReactionAdd;
export const once = false;

// Caches the first promise, so every hook of one dispatch shares a single fetch.
const memo = (fn) => {
  let promise;
  return () => (promise ??= fn());
};

// discord.js 14.15+ passes a third `details` argument, so the client is always the LAST one.
export async function execute(reaction, user, ...rest) {
  const client = rest.at(-1);
  if (user.id === client.user.id) return;
  // Partial reactions and messages still carry guildId. Never fetch them here: hooks fetch lazily.
  if (!reaction.message.guildId) return;
  if (user.partial) {
    try {
      user = await user.fetch();
    } catch {
      return;
    }
  }
  if (user.bot) return;
  const guild = client.guilds.cache.get(reaction.message.guildId);
  if (!guild) return;

  await runHooks('messageReactionAdd', {
    client,
    guild,
    reaction,
    user,
    message: reaction.message,
    emojiKey: reaction.emoji.id ?? reaction.emoji.name,
    ensureReaction: memo(async () => (reaction.partial ? reaction.fetch().catch(() => null) : reaction)),
    ensureMessage: memo(async () => (reaction.message.partial ? reaction.message.fetch().catch(() => null) : reaction.message)),
    getMember: memo(async () => guild.members.fetch(user.id).catch(() => null)),
  });
}
