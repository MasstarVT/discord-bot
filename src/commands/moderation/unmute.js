import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { deliverModReply, executeUnmute, replyModError } from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { clampText } from '../../utils/text.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);

export const data = new SlashCommandBuilder()
  .setName('unmute')
  .setDescription("Remove a member's timeout or mute role (MEE6 /unmute).")
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addUserOption((o) => o.setName('user').setDescription('Member to unmute').setRequired(true))
  .addStringOption((o) => reasonOpt(o));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);

/**
 * /unmute: removes the timeout and/or the mute role and closes stale TIMEOUT / MUTE records. A relief
 * action (no immunity check); a member who left only has their records closed.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const user = interaction.options.getUser('user', true);
  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeUnmute({
    guild: interaction.guild,
    target: user,
    moderator: interaction.user,
    reason: interaction.options.getString('reason'),
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  const inf = res.infraction;
  await deliverModReply(interaction, settings, {
    title: 'Member Unmuted',
    lines: [
      `**User:** <@${user.id}> \`${tagOf(user)}\``,
      `**Removed:** ${res.removed.join(' and ') || 'nothing (records only)'}`,
      `**Records closed:** ${res.cleared}`,
      `**Reason:** ${clampText(inf.reason, 1000)}`,
      `**Case:** #${inf.caseId}`,
    ],
    warnings: res.note ? [res.note] : [],
  });
}
