import { Events, MessageType } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageCreate;
export const once = false;

// Only real user messages enter the pipeline: join, boost, pin and thread notices never earn XP,
// get moderated, triggered or translated (R262/R263).
const USER_MESSAGE_TYPES = new Set([MessageType.Default, MessageType.Reply]);

export async function execute(message, client) {
  if (message.author.bot || message.webhookId) {
    // Other bots, webhooks and their interaction replies (such as Disboard's /bump answer, a
    // ChatInputCommand message with a webhookId) reach only the botMessage hooks, before the type filter.
    if (message.inGuild() && message.author.id !== client.user?.id) {
      await runHooks('botMessage', { client, guild: message.guild, message, channel: message.channel });
    }
    return;
  }
  if (!USER_MESSAGE_TYPES.has(message.type)) return;
  if (!message.inGuild()) {
    // A DM to the bot (modmail). There is no guild: its hooks are ungated and find the server themselves.
    if (!message.guildId) await runHooks('directMessage', { client, guild: null, message, channel: message.channel, user: message.author });
    return;
  }

  // The hook runner isolates every step and stops after ctx.stop(), so a message automod deleted
  // never reaches XP, triggers or translation (R113-R117, R264, R265).
  await runHooks('messageCreate', {
    client,
    guild: message.guild,
    message,
    member: message.member,
    channel: message.channel,
  });
}
