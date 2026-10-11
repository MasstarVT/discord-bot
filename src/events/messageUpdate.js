import { Events } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageUpdate;
export const once = false;

// When the old message was not cached, an edit is recognised by a fresh editedTimestamp.
const RECENT_EDIT_MS = 15_000;

export async function execute(oldMessage, newMessage, client) {
  if (!newMessage.guildId) return;
  if (newMessage.partial) {
    try {
      newMessage = await newMessage.fetch();
    } catch {
      return;
    }
  }
  if (!newMessage.inGuild() || newMessage.author?.bot || newMessage.webhookId) return;

  // Pins, embed unfurls and thread creation update a message without editing it (R120/R121).
  const oldKnown = !oldMessage.partial;
  const isEdit = oldKnown
    ? oldMessage.content !== newMessage.content
    : newMessage.editedTimestamp != null && Date.now() - newMessage.editedTimestamp < RECENT_EDIT_MS;
  if (!isEdit) return;

  await runHooks('messageUpdate', {
    client,
    guild: newMessage.guild,
    oldMessage,
    newMessage,
    message: newMessage,
    oldKnown,
    member: newMessage.member,
    channel: newMessage.channel,
  });
}
