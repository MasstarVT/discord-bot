import { Events, EmbedBuilder } from 'discord.js';
import { sendLog, extractMentions } from '../services/loggingService.js';

export const name = Events.MessageUpdate;
export const once = false;

export async function execute(oldMessage, newMessage, client) {
  if (!newMessage.guild) return;
  if (newMessage.author?.bot) return;

  // Discord fires MessageUpdate for embed unfurls — skip if content unchanged
  const oldContent = oldMessage.content ?? null;
  const newContent = newMessage.content ?? null;
  if (oldContent === newContent) return;

  const author  = newMessage.author;
  const channel = newMessage.channel;

  // Ghost ping: mentions that existed before but were removed
  const oldMentions = extractMentions(oldMessage);
  const newMentionSet = new Set(extractMentions(newMessage));
  const removedMentions = oldMentions.filter((m) => !newMentionSet.has(m));
  const isGhostPing = removedMentions.length > 0;

  const embed = new EmbedBuilder()
    .setColor(isGhostPing ? 0xff9800 : 0xfee75c)
    .setTitle(isGhostPing ? '👻 Ghost Ping Detected — Message Edited' : '✏️ Message Edited')
    .setTimestamp(newMessage.editedTimestamp ?? Date.now());

  if (author) {
    embed
      .setAuthor({ name: author.tag, iconURL: author.displayAvatarURL() })
      .addFields(
        { name: 'Author',  value: `<@${author.id}> \`${author.tag}\``, inline: true },
        { name: 'Channel', value: `<#${channel.id}>`,                  inline: true },
      )
      .setFooter({ text: `User ID: ${author.id} · Msg ID: ${newMessage.id}` });
  } else {
    embed
      .addFields({ name: 'Channel', value: `<#${channel.id}>`, inline: true })
      .setFooter({ text: `Msg ID: ${newMessage.id}` });
  }

  embed.addFields(
    { name: 'Before', value: (oldContent?.slice(0, 512) || '*Not cached*') },
    { name: 'After',  value: (newContent?.slice(0, 512) || '*Empty*')      },
  );

  if (isGhostPing) {
    embed.addFields({
      name:  '⚠️ Removed Mentions',
      value: removedMentions.join('\n').slice(0, 512),
    });
  }

  // Jump link
  embed.addFields({
    name:  'Jump',
    value: `[View Message](${newMessage.url})`,
    inline: true,
  });

  await sendLog(newMessage.guild, 'MESSAGE', embed, client);
}
