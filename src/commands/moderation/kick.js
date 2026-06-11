import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, assertHierarchy } from '../../services/moderationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('kick')
  .setDescription('Kick a member from the server.')
  .setDefaultMemberPermissions(PermissionFlagsBits.KickMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to kick').setRequired(true))
  .addStringOption((o) => o.setName('reason').setDescription('Reason for the kick'))
  .addStringOption((o) => o.setName('proof').setDescription('Proof URL'));

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target   = interaction.options.getUser('user', true);
  const reason   = interaction.options.getString('reason') ?? 'No reason provided';
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
    const infraction = await createInfraction(guild, target, interaction.user, 'KICK', {
      reason,
      proofUrl,
      client,
    });

    await targetMember.kick(`[Case #${infraction.caseId}] ${reason} — ${interaction.user.tag}`);

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Reason:** ${reason}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('User Kicked', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Kick Failed', err.message)] });
  }
}
