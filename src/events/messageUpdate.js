import { Events, MessageFlags } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageUpdate;
export const once = false;

// When the old message was not cached, an edit is recognised by a fresh editedTimestamp.
const RECENT_EDIT_MS = 15_000;

/** An edit by the rule below: a fresh editedTimestamp (discord.js patches it from the gateway payload, or leaves it null). */
const recentlyEdited = (message) => message.editedTimestamp != null && Date.now() - message.editedTimestamp < RECENT_EDIT_MS;

export async function execute(oldMessage, newMessage, client) {
  if (!newMessage.guildId) return;
  // An uncached message that was not freshly edited (an embed unfurl, a pin) is no edit: skip the REST fetch.
  if (newMessage.partial && oldMessage.partial && !recentlyEdited(newMessage)) return;
  if (newMessage.partial) {
    try {
      newMessage = await newMessage.fetch();
    } catch {
      return;
    }
  }
  if (!newMessage.inGuild()) return;
  if (newMessage.author?.bot || newMessage.webhookId) {
    // A bot's deferred interaction reply arrives as an edit of its "thinking" message (a bump site may answer so):
    // botMessage hooks see bot edits too, flagged `edited`; `wasLoading` says the edit turned the placeholder into the
    // answer (null when the old message was not cached).
    if (newMessage.author?.id !== client.user?.id) {
      const wasLoading = oldMessage.partial ? null : oldMessage.flags?.has(MessageFlags.Loading) === true;
      await runHooks('botMessage', { client, guild: newMessage.guild, message: newMessage, channel: newMessage.channel, edited: true, wasLoading });
    }
    return;
  }

  // Pins, embed unfurls and thread creation update a message without editing it (R120/R121).
  const oldKnown = !oldMessage.partial;
  const isEdit = oldKnown
    ? oldMessage.content !== newMessage.content
    : recentlyEdited(newMessage);
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
