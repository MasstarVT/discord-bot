import { Events, EmbedBuilder } from 'discord.js';
import { sendLog, channelTypeLabel } from '../services/loggingService.js';

export const name = Events.ChannelDelete;
export const once = false;

export async function execute(channel, client) {
  if (!channel.guild) return;

  const embed = new EmbedBuilder()
    .setColor(0xed4245)
    .setTitle('🗑️ Channel Deleted')
    .addFields(
      { name: 'Name', value: `\`${channel.name}\``,       inline: true },
      { name: 'Type', value: channelTypeLabel(channel.type), inline: true },
    )
    .setFooter({ text: `Channel ID: ${channel.id}` })
    .setTimestamp();

  if (channel.parent) {
    embed.addFields({ name: 'Category', value: channel.parent.name, inline: true });
  }

  await sendLog(channel.guild, 'SERVER', embed, client);
}
