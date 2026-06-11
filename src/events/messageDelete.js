import { Events, EmbedBuilder } from 'discord.js';
import { sendLog, extractMentions } from '../services/loggingService.js';

export const name = Events.MessageDelete;
export const once = false;

export async function execute(message, client) {
  // Skip DMs and uncached guild info
  if (!message.guild) return;
  // Skip bot-authored messages
  if (message.author?.bot) return;

  const isPartial = message.partial;
  const author    = message.author;
  const content   = message.content?.slice(0, 1024) || null;
  const channel   = message.channel;

  const mentions   = extractMentions(message);
  const isGhostPing = mentions.length > 0;

  const embed = new EmbedBuilder()
    .setColor(isGhostPing ? 0xff9800 : 0xed4245)
    .setTitle(isGhostPing ? '👻 Ghost Ping Detected — Message Deleted' : '🗑️ Message Deleted')
    .setTimestamp();

  if (author) {
    embed
      .setAuthor({ name: author.tag, iconURL: author.displayAvatarURL() })
      .addFields(
        { name: 'Author',  value: `<@${author.id}> \`${author.tag}\``, inline: true },
        { name: 'Channel', value: `<#${channel.id}>`,                  inline: true },
      )
      .setFooter({ text: `User ID: ${author.id} · Msg ID: ${message.id}` });
  } else {
    embed
      .addFields({ name: 'Channel', value: `<#${channel.id}>`, inline: true })
      .setFooter({ text: `Msg ID: ${message.id}` });
  }

  if (isPartial) {
    embed.addFields({ name: 'Content', value: '*Message was not cached — content unavailable.*' });
  } else if (content) {
    embed.addFields({ name: 'Content', value: content });
  } else {
    embed.addFields({ name: 'Content', value: '*[No text content]*' });
  }

  if (message.attachments?.size > 0) {
    const attachList = [...message.attachments.values()]
      .map((a) => `[${a.name}](${a.proxyURL})`)
      .join('\n')
      .slice(0, 512);
    embed.addFields({ name: `Attachments (${message.attachments.size})`, value: attachList });
  }

  if (isGhostPing) {
    embed.addFields({
      name:  '⚠️ Ghost Pinged',
      value: mentions.join('\n').slice(0, 512),
    });
  }

  await sendLog(message.guild, 'MESSAGE', embed, client);
}
