import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.GuildEmojiCreate;
export const once = false;

export async function execute(emoji, client) {
  await runHooks('emojiCreate', { client, guild: emoji.guild, emoji });
}
