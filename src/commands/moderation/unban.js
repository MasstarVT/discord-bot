import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, resolveInfractions } from '../../services/moderationService.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('unban')
  .setDescription('Remove a ban from a user by their ID.')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addStringOption((o) =>
    o.setName('user_id').setDescription('The Discord user ID of the banned user').setRequired(true)
  )
  .addStringOption((o) => o.setName('reason').setDescription('Reason for the unban'));

export async function execute(interaction, client) {
  // Defer ephemeral so validation errors stay private; success will followUp publicly
  await interaction.deferReply({ ephemeral: true });

  const userId = interaction.options.getString('user_id', true).trim();
  const reason = interaction.options.getString('reason') ?? 'No reason provided';
  const guild  = interaction.guild;

  // Validate it looks like a snowflake
  if (!/^\d{17,19}$/.test(userId)) {
    return interaction.editReply({ embeds: [errorEmbed('Invalid ID', 'Please provide a valid Discord user ID (17–19 digits).')] });
  }

  // Verify the ban exists
  const ban = await guild.bans.fetch(userId).catch(() => null);
  if (!ban) {
    return interaction.editReply({ embeds: [errorEmbed('Not Banned', `User \`${userId}\` is not currently banned in this server.`)] });
  }

  try {
    await guild.bans.remove(userId, `[Unban] ${reason} — ${interaction.user.tag}`);

    await resolveInfractions(guild.id, userId, ['BAN', 'SOFTBAN']);

    const infraction = await createInfraction(guild, ban.user, interaction.user, 'UNBAN', {
      reason,
      client,
      silent: true,
    });

    const desc = [
      `**User:** <@${userId}> \`${ban.user.tag}\``,
      `**Reason:** ${reason}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('User Unbanned', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Unban Failed', err.message)] });
  }
}
