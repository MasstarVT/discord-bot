import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, assertHierarchy } from '../../services/moderationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';
import prisma from '../../database/client.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('warn')
  .setDescription('Issue a formal warning to a member.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to warn').setRequired(true))
  .addStringOption((o) => o.setName('reason').setDescription('Reason for the warning').setRequired(true))
  .addStringOption((o) => o.setName('proof').setDescription('Proof URL'));

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target   = interaction.options.getUser('user', true);
  const reason   = interaction.options.getString('reason', true);
  const proofUrl = interaction.options.getString('proof') ?? undefined;
  const guild    = interaction.guild;

  const targetMember = await guild.members.fetch(target.id).catch(() => null);
  if (!targetMember) {
    return interaction.editReply({ embeds: [errorEmbed('Not Found', 'That user is not in this server.')] });
  }
  try {
    assertHierarchy(guild, targetMember, interaction.member);
  } catch (err) {
    return interaction.editReply({ embeds: [errorEmbed('Hierarchy Error', err.message)] });
  }

  try {
    const infraction = await createInfraction(guild, target, interaction.user, 'WARN', {
      reason,
      proofUrl,
      client,
    });

    // Fetch total warn count for display
    const totalWarns = await prisma.infraction.count({
      where: { guildId: guild.id, targetUserId: target.id, type: 'WARN' },
    });

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Reason:** ${reason}`,
      `**Total Warnings:** ${totalWarns}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('Warning Issued', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Warn Failed', err.message)] });
  }
}
