import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildEmojiUpdate;
export const once = false;

export async function execute(oldEmoji, newEmoji, client) {
  await runHooks('emojiUpdate', { client, guild: newEmoji.guild, oldEmoji, newEmoji, emoji: newEmoji });
}
