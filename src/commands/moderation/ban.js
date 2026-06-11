import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, assertHierarchy } from '../../services/moderationService.js';
import { parseDuration, formatDuration } from '../../utils/parseDuration.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

export const data = new SlashCommandBuilder()
  .setName('ban')
  .setDescription('Permanently ban a user from the server.')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addUserOption((o) => o.setName('user').setDescription('User to ban').setRequired(true))
  .addStringOption((o) => o.setName('reason').setDescription('Reason for the ban'))
  .addStringOption((o) =>
    o.setName('duration')
      .setDescription('Temporary ban duration (e.g. 7d, 24h). Leave empty for permanent.')
  )
  .addIntegerOption((o) =>
    o.setName('delete_days')
      .setDescription('Days of message history to delete (0–7)')
      .setMinValue(0)
      .setMaxValue(7)
  )
  .addStringOption((o) => o.setName('proof').setDescription('Proof URL (image/link)'));

export async function execute(interaction, client) {
  // Defer ephemeral so validation errors stay private; success will followUp publicly
  await interaction.deferReply({ ephemeral: true });

  const target    = interaction.options.getUser('user', true);
  const reason    = interaction.options.getString('reason') ?? 'No reason provided';
  const durationStr = interaction.options.getString('duration');
  const deleteDays  = interaction.options.getInteger('delete_days') ?? 0;
  const proofUrl  = interaction.options.getString('proof') ?? undefined;
  const guild     = interaction.guild;

  const durationMs = durationStr ? parseDuration(durationStr) : null;
  if (durationStr && !durationMs) {
    return interaction.editReply({ embeds: [errorEmbed('Invalid Duration', `Could not parse \`${durationStr}\`. Try \`7d\`, \`24h\`, \`30m\`.`)] });
  }

  // Resolve member for hierarchy check (may not be in guild)
  const targetMember = await guild.members.fetch(target.id).catch(() => null);
  try {
    assertHierarchy(guild, targetMember, interaction.member);
  } catch (err) {
    return interaction.editReply({ embeds: [errorEmbed('Hierarchy Error', err.message)] });
  }

  // Bail out early if the user is already banned — avoids an orphaned infraction
  const existingBan = await guild.bans.fetch(target.id).catch(() => null);
  if (existingBan) {
    return interaction.editReply({
      embeds: [errorEmbed('Already Banned', `<@${target.id}> \`${target.tag}\` is already banned from this server.`)],
    });
  }

  try {
    // DM before ban so the channel still exists when we try
    const infraction = await createInfraction(guild, target, interaction.user, 'BAN', {
      reason,
      proofUrl,
      durationMs,
      client,
    });

    await guild.bans.create(target.id, {
      reason:             `[Case #${infraction.caseId}] ${reason} — ${interaction.user.tag}`,
      deleteMessageDays:  deleteDays,
    });

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Reason:** ${reason}`,
      `**Duration:** ${durationMs ? formatDuration(durationMs) : 'Permanent'}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('User Banned', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Ban Failed', err.message)] });
  }
}
