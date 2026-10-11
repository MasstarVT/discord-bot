import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { deliverModReply, executeModAction, replyModError } from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed } from '../../utils/embedBuilder.js';
import { formatDurationLong, parseDuration } from '../../utils/parseDuration.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);
const proofOpt = (o) => o.setName('proof').setDescription('Proof link (http/https)').setMaxLength(1000);

export const data = new SlashCommandBuilder()
  .setName('timeout')
  .setDescription('Time out a member; over 28 days uses the mute role (MEE6 /mute, /tempmute).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to time out').setRequired(true))
  .addStringOption((o) => o.setName('duration').setDescription('Length, e.g. 10m, 2h, 7d (over 28d needs a mute role)').setRequired(true).setMaxLength(32))
  .addStringOption((o) => reasonOpt(o))
  .addStringOption((o) => proofOpt(o));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);

/**
 * /timeout: a native timeout up to 28 days; the service switches to the mute role beyond that (when one is set).
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const user = interaction.options.getUser('user', true);
  const durationStr = interaction.options.getString('duration', true);

  const durationMs = parseDuration(durationStr);
  if (!durationMs) {
    await interaction.editReply({
      embeds: [errorEmbed('Invalid Duration', `Couldn't read \`${clampText(durationStr.replace(/`/g, "'"), 32)}\`. Use something like \`12h\`, \`7d\` or \`1mo\`.`)],
    });
    return;
  }

  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeModAction({
    guild: interaction.guild,
    target: user,
    moderator: interaction.user,
    action: 'TIMEOUT',
    reason: interaction.options.getString('reason'),
    durationMs,
    proofUrl: interaction.options.getString('proof'),
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  const inf = res.infraction;
  const lines = [
    `**User:** <@${user.id}> \`${tagOf(user)}\``,
    `**Duration:** ${formatDurationLong(durationMs)}`,
  ];
  if (inf.expiresAt) lines.push(`**Ends:** ${discordTimestamp(inf.expiresAt, 'R')}`);
  lines.push(`**Reason:** ${clampText(inf.reason, 1000)}`, `**Case:** #${inf.caseId}`);

  const warnings = [...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])];
  await deliverModReply(interaction, settings, {
    title: res.action === 'MUTE' ? 'Member Muted' : 'Member Timed Out',
    lines,
    warnings,
  });
}
