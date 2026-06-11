import { Events, EmbedBuilder } from 'discord.js';
import { sendLog } from '../services/loggingService.js';
import { cacheGuildInvites } from '../services/inviteTracker.js';

export const name = Events.InviteDelete;
export const once = false;

export async function execute(invite, client) {
  if (!invite.guild) return;

  // Refresh cache so deleted invite is no longer tracked
  await cacheGuildInvites(invite.guild);

  const embed = new EmbedBuilder()
    .setColor(0xed4245)
    .setTitle('🔗 Invite Deleted / Expired')
    .addFields(
      { name: 'Code',    value: `\`${invite.code}\``,                               inline: true },
      { name: 'Channel', value: invite.channel ? `<#${invite.channel.id}>` : 'Unknown', inline: true },
    )
    .setFooter({ text: `Code: ${invite.code}` })
    .setTimestamp();

  await sendLog(invite.guild, 'SERVER', embed, client);
}
