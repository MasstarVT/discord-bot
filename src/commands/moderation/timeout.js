import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { createInfraction, assertHierarchy } from '../../services/moderationService.js';
import { parseDuration, formatDuration } from '../../utils/parseDuration.js';
import { successEmbed, errorEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

// Discord timeout maximum: 28 days
const MAX_TIMEOUT_MS = 28 * 24 * 60 * 60 * 1_000;

export const data = new SlashCommandBuilder()
  .setName('timeout')
  .setDescription('Temporarily mute a member using Discord\'s native timeout.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to timeout').setRequired(true))
  .addStringOption((o) =>
    o.setName('duration')
      .setDescription('Duration (e.g. 5m, 1h, 7d — max 28d)')
      .setRequired(true)
  )
  .addStringOption((o) => o.setName('reason').setDescription('Reason'))
  .addStringOption((o) => o.setName('proof').setDescription('Proof URL'));

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const target      = interaction.options.getUser('user', true);
  const durationStr = interaction.options.getString('duration', true);
  const reason      = interaction.options.getString('reason') ?? 'No reason provided';
  const proofUrl    = interaction.options.getString('proof') ?? undefined;
  const guild       = interaction.guild;

  const durationMs = parseDuration(durationStr);
  if (!durationMs) {
    return interaction.editReply({ embeds: [errorEmbed('Invalid Duration', `Could not parse \`${durationStr}\`.`)] });
  }
  if (durationMs > MAX_TIMEOUT_MS) {
    return interaction.editReply({ embeds: [errorEmbed('Duration Too Long', 'Discord timeouts have a maximum of 28 days.')] });
  }

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
    const infraction = await createInfraction(guild, target, interaction.user, 'TIMEOUT', {
      reason,
      proofUrl,
      durationMs,
      client,
    });

    await targetMember.timeout(durationMs, `[Case #${infraction.caseId}] ${reason} — ${interaction.user.tag}`);

    const desc = [
      `**User:** <@${target.id}> \`${target.tag}\``,
      `**Duration:** ${formatDuration(durationMs)}`,
      `**Expires:** <t:${Math.floor((Date.now() + durationMs) / 1000)}:R>`,
      `**Reason:** ${reason}`,
      `**Case:** #${infraction.caseId}`,
    ].join('\n');

    await interaction.deleteReply();
    await interaction.followUp({ embeds: [successEmbed('User Timed Out', desc)] });
  } catch (err) {
    await interaction.editReply({ embeds: [errorEmbed('Timeout Failed', err.message)] });
  }
}
