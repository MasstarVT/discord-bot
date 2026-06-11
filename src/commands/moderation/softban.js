import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, assertHierarchy } from '../../services/moderationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('softban')
  .setDescription('Ban then immediately unban a user to delete their recent messages.')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addUserOption((o) => o.setName('user').setDescription('User to softban').setRequired(true))
  .addStringOption((o) => o.setName('reason').setDescription('Reason for the softban'))
  .addIntegerOption((o) =>
    o.setName('delete_days')
      .setDescription('Days of message history to delete (0–7, default 7)')
      .setMinValue(0)
      .setMaxValue(7)
  )
  .addStringOption((o) => o.setName('proof').setDescription('Proof URL'));

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target     = interaction.options.getUser('user', true);
  const reason     = interaction.options.getString('reason') ?? 'No reason provided';
  const deleteDays = interaction.options.getInteger('delete_days') ?? 7;
  const proofUrl   = interaction.options.getString('proof') ?? undefined;
  const guild      = interaction.guild;

  const targetMember = await guild.members.fetch(target.id).catch(() => null);
  try {
    assertHierarchy(guild, targetMember, interaction.member);
  } catch (err) {
    return interaction.editReply({ embeds: [errorEmbed('Hierarchy Error', err.message)] });
  }

  try {
    const infraction = await createInfraction(guild, target, interaction.user, 'SOFTBAN', {
      reason,
      proofUrl,
      client,
    });

    await guild.bans.create(target.id, {
      reason:            `[Case #${infraction.caseId}] ${reason} — ${interaction.user.tag}`,
      deleteMessageDays: deleteDays,
    });
    await guild.bans.remove(target.id, 'Softban — immediate unban').catch(() => null);

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Reason:** ${reason}`,
      `**Messages Deleted:** ${deleteDays} day(s)`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('User Softbanned', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Softban Failed', err.message)] });
  }
}
