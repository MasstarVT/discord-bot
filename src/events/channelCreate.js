import { Events, EmbedBuilder } from 'discord.js';
import { sendLog, channelTypeLabel } from '../services/loggingService.js';

export const name = Events.ChannelCreate;
export const once = false;

export async function execute(channel, client) {
  if (!channel.guild) return;

  const embed = new EmbedBuilder()
    .setColor(0x57f287)
    .setTitle('📢 Channel Created')
    .addFields(
      { name: 'Name',     value: `<#${channel.id}> \`${channel.name}\``,    inline: true },
      { name: 'Type',     value: channelTypeLabel(channel.type),            inline: true },
    )
    .setFooter({ text: `Channel ID: ${channel.id}` })
    .setTimestamp();

  if (channel.parent) {
    embed.addFields({ name: 'Category', value: channel.parent.name, inline: true });
  }
  if (channel.topic) {
    embed.addFields({ name: 'Topic', value: channel.topic.slice(0, 256) });
  }

  await sendLog(channel.guild, 'SERVER', embed, client);
}
