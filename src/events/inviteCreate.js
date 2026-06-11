import { Events, EmbedBuilder } from 'discord.js';
import { sendLog } from '../services/loggingService.js';
import { cacheGuildInvites } from '../services/inviteTracker.js';

export const name = Events.InviteCreate;
export const once = false;

export async function execute(invite, client) {
  if (!invite.guild) return;

  // Keep invite cache fresh so the next guildMemberAdd can diff correctly
  await cacheGuildInvites(invite.guild);

  const embed = new EmbedBuilder()
    .setColor(0x5865f2)
    .setTitle('🔗 Invite Created')
    .addFields(
      { name: 'Code',     value: `discord.gg/${invite.code}`,                                                         inline: true },
      { name: 'Channel',  value: invite.channel ? `<#${invite.channel.id}>` : 'Unknown',                              inline: true },
      { name: 'Inviter',  value: invite.inviter  ? `<@${invite.inviter.id}> \`${invite.inviter.tag}\`` : 'Unknown',   inline: true },
      { name: 'Max Uses', value: invite.maxUses ? String(invite.maxUses) : '∞',                                       inline: true },
      { name: 'Expires',  value: invite.expiresAt ? `<t:${Math.floor(invite.expiresAt.getTime() / 1000)}:R>` : 'Never', inline: true },
      { name: 'Temporary',value: invite.temporary ? 'Yes' : 'No',                                                     inline: true },
    )
    .setFooter({ text: `Code: ${invite.code}` })
    .setTimestamp();

  await sendLog(invite.guild, 'SERVER', embed, client);
}
