import { Events, MessageType } from 'discord.js';
import { runHooks } from '../handlers/hookRegistry.js';

export const name = Events.MessageCreate;
export const once = false;

// Only real user messages enter the pipeline: join, boost, pin and thread notices never earn XP,
// get moderated, triggered or translated (R262/R263).
const USER_MESSAGE_TYPES = new Set([MessageType.Default, MessageType.Reply]);

export async function execute(message, client) {
  if (!message.inGuild() || message.author.bot || message.webhookId) return;
  if (!USER_MESSAGE_TYPES.has(message.type)) return;

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
