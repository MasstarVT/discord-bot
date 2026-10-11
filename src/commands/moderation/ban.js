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
  .setName('ban')
  .setDescription('Ban a user, optionally for a set time (MEE6 /ban, /tempban).')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addUserOption((o) => o.setName('user').setDescription('User to ban (members or users who left)').setRequired(true))
  .addStringOption((o) => reasonOpt(o))
  .addStringOption((o) => o.setName('duration').setDescription('Temporary ban length, e.g. 12h or 7d (max 1 year). Omit for permanent').setMaxLength(32))
  .addIntegerOption((o) => o.setName('delete_days').setDescription('Days of their messages to delete (0-7, default 0)').setMinValue(0).setMaxValue(7))
  .addStringOption((o) => proofOpt(o));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);

/**
 * /ban: the Discord action runs first inside executeModAction; nothing is recorded when it fails.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const user = interaction.options.getUser('user', true);
  const reason = interaction.options.getString('reason');
  const durationStr = interaction.options.getString('duration');
  const deleteDays = interaction.options.getInteger('delete_days') ?? 0;
  const proof = interaction.options.getString('proof');

  // A duration that was typed but cannot be read must never silently become a permanent ban.
  let durationMs = null;
  if (durationStr !== null) {
    durationMs = parseDuration(durationStr);
    if (!durationMs) {
      await interaction.editReply({
        embeds: [errorEmbed('Invalid Duration', `Couldn't read \`${clampText(durationStr.replace(/`/g, "'"), 32)}\`. Use something like \`12h\`, \`7d\` or \`1mo\`.`)],
      });
      return;
    }
  }

  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeModAction({
    guild: interaction.guild,
    target: user,
    moderator: interaction.user,
    action: durationMs ? 'TEMPBAN' : 'BAN',
    reason,
    durationMs,
    deleteMessageSeconds: deleteDays * 86_400,
    proofUrl: proof,
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  const inf = res.infraction;
  const temporary = res.action === 'TEMPBAN';
  const lines = [
    `**User:** <@${user.id}> \`${tagOf(user)}\``,
    `**Reason:** ${clampText(inf.reason, 1000)}`,
    `**Duration:** ${durationMs ? formatDurationLong(durationMs) : 'Permanent'}`,
  ];
  if (temporary && inf.expiresAt) lines.push(`**Ends:** ${discordTimestamp(inf.expiresAt, 'R')}`);
  if (deleteDays > 0) lines.push(`**Messages deleted:** ${deleteDays} day(s)`);
  lines.push(`**Case:** #${inf.caseId}`);

  const warnings = [...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])];
  await deliverModReply(interaction, settings, {
    title: temporary ? 'User Temporarily Banned' : 'User Banned',
    lines,
    warnings,
  });
}
