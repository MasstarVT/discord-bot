import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildEmojiDelete;
export const once = false;

export async function execute(emoji, client) {
  await runHooks('emojiDelete', { client, guild: emoji.guild, emoji });
}
