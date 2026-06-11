import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, resolveInfractions, assertHierarchy } from '../../services/moderationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('unmute')
  .setDescription('Remove an active timeout from a member.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to unmute').setRequired(true))
  .addStringOption((o) => o.setName('reason').setDescription('Reason'));

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target = interaction.options.getUser('user', true);
  const reason = interaction.options.getString('reason') ?? 'No reason provided';
  const guild  = interaction.guild;

  const targetMember = await guild.members.fetch(target.id).catch(() => null);
  if (!targetMember) {
    return interaction.editReply({ embeds: [errorEmbed('Not Found', 'That user is not in this server.')] });
  }

  if (!targetMember.isCommunicationDisabled()) {
    return interaction.editReply({ embeds: [errorEmbed('Not Timed Out', 'That member does not have an active timeout.')] });
  }

  try {
    assertHierarchy(guild, targetMember, interaction.member);
  } catch (err) {
    return interaction.editReply({ embeds: [errorEmbed('Hierarchy Error', err.message)] });
  }

  try {
    await targetMember.timeout(null, `[Unmute] ${reason} — ${interaction.user.tag}`);

    await resolveInfractions(guild.id, target.id, ['TIMEOUT', 'MUTE']);

    const infraction = await createInfraction(guild, target, interaction.user, 'UNMUTE', {
      reason,
      client,
      silent: true,
    });

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Reason:** ${reason}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('Timeout Removed', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Unmute Failed', err.message)] });
  }
}
