import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { TIER_ACTION_LABEL, deliverModReply, executeModAction, replyModError } from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import logger from '../../utils/logger.js';
import { clampText } from '../../utils/text.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);
const proofOpt = (o) => o.setName('proof').setDescription('Proof link (http/https)').setMaxLength(1000);

export const data = new SlashCommandBuilder()
  .setName('warn')
  .setDescription('Warn a member; warnings can trigger automatic actions (MEE6 /warn).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to warn').setRequired(true))
  .addStringOption((o) => reasonOpt(o, 'Reason for the warning').setRequired(true))
  .addStringOption((o) => proofOpt(o));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);

/**
 * /warn: records an active WARN (the service runs escalation) and reports the active total and any
 * escalation that fired.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const user = interaction.options.getUser('user', true);
  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeModAction({
    guild: interaction.guild,
    target: user,
    moderator: interaction.user,
    action: 'WARN',
    reason: interaction.options.getString('reason', true),
    proofUrl: interaction.options.getString('proof'),
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  // The count follows the action, so it includes this warning (and ignores pardoned ones).
  let active = null;
  try {
    active = await prisma.infraction.count({
      where: { guildId: interaction.guildId, targetUserId: user.id, type: 'WARN', active: true },
    });
  } catch (err) {
    logger.warn(`/warn: could not count active warnings: ${err?.message ?? err}`);
  }

  const inf = res.infraction;
  const lines = [
    `**User:** <@${user.id}> \`${tagOf(user)}\``,
    `**Reason:** ${clampText(inf.reason, 1000)}`,
  ];
  if (active !== null) lines.push(`**Active warnings:** ${active}`);
  lines.push(`**Case:** #${inf.caseId}`);

  const warnings = [...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])];
  const e = res.escalation;
  if (e?.ok) {
    lines.push(`**Escalation T${e.tierId}:** ${TIER_ACTION_LABEL[e.action] ?? e.action} applied (case #${e.caseId})`);
  } else if (e) {
    warnings.push(`Escalation T${e.tierId} (${TIER_ACTION_LABEL[e.action] ?? e.action}) could not be applied: ${clampText(e.error ?? 'unknown error', 300)}`);
  }

  await deliverModReply(interaction, settings, { title: 'Warning Issued', lines, warnings });
}
