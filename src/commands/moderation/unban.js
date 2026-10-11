import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { deliverModReply, executeUnban, replyModError } from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { clampText } from '../../utils/text.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);

export const data = new SlashCommandBuilder()
  .setName('unban')
  .setDescription('Lift a ban by user ID (MEE6 /unban).')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addStringOption((o) => o.setName('user_id').setDescription('ID or mention of the banned user').setRequired(true).setMinLength(17).setMaxLength(32))
  .addStringOption((o) => reasonOpt(o));

/**
 * /unban: a relief action (no immunity check). The service validates the ID, lifts the ban, closes the
 * ban records, cancels a pending expiry job and records the UNBAN.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const userId = interaction.options.getString('user_id', true).replace(/[<@!>\s]/g, '');
  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeUnban({
    guild: interaction.guild,
    userId,
    moderator: interaction.user,
    reason: interaction.options.getString('reason'),
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  const tag = clampText(String(res.user?.tag ?? res.user?.username ?? userId).replace(/`/g, "'"), 100);
  const lines = [
    `**User:** <@${userId}> \`${tag}\``,
    `**Reason:** ${clampText(res.infraction.reason, 1000)}`,
    `**Case:** #${res.infraction.caseId}`,
  ];
  if (res.cleared > 1) lines.push(`**Ban records closed:** ${res.cleared}`);
  await deliverModReply(interaction, settings, { title: 'User Unbanned', lines });
}
