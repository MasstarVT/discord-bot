import { SlashCommandBuilder, PermissionFlagsBits, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { errorEmbed, successEmbed } from '../../utils/embedBuilder.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const ACTION_COLORS = {
  WARN: COLORS.WARNING, TIMEOUT: 0xff9800, MUTE: 0xff9800,
  KICK: 0xff5722, SOFTBAN: 0xf44336, BAN: 0xd32f2f,
  UNBAN: COLORS.SUCCESS, UNMUTE: COLORS.SUCCESS, NOTE: COLORS.INFO,
};
const ACTION_EMOJI = {
  WARN: '⚠️', TIMEOUT: '⏱️', MUTE: '🔇', KICK: '👢',
  SOFTBAN: '🔨', BAN: '🔨', UNBAN: '✅', UNMUTE: '🔊', NOTE: '📝',
};

export const data = new SlashCommandBuilder()
  .setName('case')
  .setDescription('View or edit a moderation case.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((sub) =>
    sub.setName('view')
      .setDescription('View details of a case.')
      .addIntegerOption((o) => o.setName('id').setDescription('Case ID').setRequired(true).setMinValue(1))
  )
  .addSubcommand((sub) =>
    sub.setName('reason')
      .setDescription('Update the reason for a case.')
      .addIntegerOption((o) => o.setName('id').setDescription('Case ID').setRequired(true).setMinValue(1))
      .addStringOption((o) => o.setName('reason').setDescription('New reason').setRequired(true))
  )
  .addSubcommand((sub) =>
    sub.setName('proof')
      .setDescription('Attach or update the proof URL for a case.')
      .addIntegerOption((o) => o.setName('id').setDescription('Case ID').setRequired(true).setMinValue(1))
      .addStringOption((o) => o.setName('url').setDescription('Proof URL').setRequired(true))
  );

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  const sub    = interaction.options.getSubcommand();
  const caseId = interaction.options.getInteger('id', true);
  const guild  = interaction.guild;

  const infraction = await prisma.infraction.findUnique({
    where: { guildId_caseId: { guildId: guild.id, caseId } },
  });

  if (!infraction) {
    return interaction.editReply({ embeds: [errorEmbed('Not Found', `Case #${caseId} does not exist in this server.`)] });
  }

  // ── View ────────────────────────────────────────────────────────────────────
  if (sub === 'view') {
    const target = await client.users.fetch(infraction.targetUserId).catch(() => ({ id: infraction.targetUserId, tag: 'Unknown User' }));
    const mod    = await client.users.fetch(infraction.moderatorId).catch(() => ({ id: infraction.moderatorId, tag: 'Unknown Mod' }));
    const emoji  = ACTION_EMOJI[infraction.type]  ?? '📋';
    const color  = ACTION_COLORS[infraction.type] ?? COLORS.NEUTRAL;

    const embed = new EmbedBuilder()
      .setColor(color)
      .setTitle(`${emoji} Case #${caseId} — ${infraction.type}`)
      .addFields(
        { name: 'Target',    value: `<@${target.id}> \`${target.tag}\``, inline: true },
        { name: 'Moderator', value: `<@${mod.id}> \`${mod.tag}\``,       inline: true },
        { name: 'Reason',    value: infraction.reason || 'None' },
        { name: 'Active',    value: infraction.active ? 'Yes' : 'No',    inline: true },
        { name: 'Created',   value: `<t:${Math.floor(infraction.createdAt.getTime() / 1000)}:F>`, inline: true },
      )
      .setFooter({ text: `User ID: ${infraction.targetUserId}` })
      .setTimestamp();

    if (infraction.expiresAt) {
      embed.addFields({ name: 'Expires', value: `<t:${Math.floor(infraction.expiresAt.getTime() / 1000)}:R>` });
    }
    if (infraction.proofUrl) {
      embed.addFields({ name: 'Proof', value: infraction.proofUrl });
    }

    await interaction.deleteReply();
    return interaction.followUp({ embeds: [embed] });
  }

  // ── Reason update ────────────────────────────────────────────────────────────
  if (sub === 'reason') {
    const newReason = interaction.options.getString('reason', true);
    await prisma.infraction.update({
      where: { guildId_caseId: { guildId: guild.id, caseId } },
      data:  { reason: newReason },
    });
    await interaction.deleteReply();
    return interaction.followUp({
      embeds: [successEmbed('Case Updated', `Case #${caseId} reason updated to:\n> ${newReason}`)],
    });
  }

  // ── Proof update ─────────────────────────────────────────────────────────────
  if (sub === 'proof') {
    const url = interaction.options.getString('url', true);
    await prisma.infraction.update({
      where: { guildId_caseId: { guildId: guild.id, caseId } },
      data:  { proofUrl: url },
    });
    await interaction.deleteReply();
    return interaction.followUp({
      embeds: [successEmbed('Case Updated', `Case #${caseId} proof URL updated.`)],
    });
  }
}
