import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { deliverModReply, executeModAction, replyModError } from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { clampText } from '../../utils/text.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const reasonOpt = (o, desc = 'Reason (audit log and DM)') => o.setName('reason').setDescription(desc).setMaxLength(512);
const proofOpt = (o) => o.setName('proof').setDescription('Proof link (http/https)').setMaxLength(1000);

export const data = new SlashCommandBuilder()
  .setName('softban')
  .setDescription('Ban and instantly unban a user to delete their recent messages.')
  .setDefaultMemberPermissions(PermissionFlagsBits.BanMembers)
  .addUserOption((o) => o.setName('user').setDescription('User to softban').setRequired(true))
  .addStringOption((o) => reasonOpt(o))
  .addIntegerOption((o) => o.setName('delete_days').setDescription('Days of their messages to delete (1-7, default 7)').setMinValue(1).setMaxValue(7))
  .addStringOption((o) => proofOpt(o));

const tagOf = (user) => clampText(String(user?.tag ?? user?.username ?? user?.id ?? 'unknown').replace(/`/g, "'"), 100);

/**
 * /softban: an already-banned user is refused (ALREADY) so the standing ban is never lifted (R029).
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @returns {Promise<void>}
 */
export async function execute(interaction) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const user = interaction.options.getUser('user', true);
  const deleteDays = interaction.options.getInteger('delete_days') ?? 7;
  const settings = await getSettingsSafe(interaction.guildId);
  const res = await executeModAction({
    guild: interaction.guild,
    target: user,
    moderator: interaction.user,
    action: 'SOFTBAN',
    reason: interaction.options.getString('reason'),
    deleteMessageSeconds: deleteDays * 86_400,
    proofUrl: interaction.options.getString('proof'),
    source: 'command',
  });
  if (!res.ok) {
    await replyModError(interaction, res);
    return;
  }

  const inf = res.infraction;
  // A softban whose automatic unban failed is stored as an active BAN (the service adds the warning).
  const stuck = inf.type === 'BAN';
  const warnings = [...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])];
  await deliverModReply(interaction, settings, {
    title: stuck ? 'User Banned (Softban Incomplete)' : 'User Softbanned',
    lines: [
      `**User:** <@${user.id}> \`${tagOf(user)}\``,
      `**Reason:** ${clampText(inf.reason, 1000)}`,
      `**Messages deleted:** ${deleteDays} day(s)`,
      `**Case:** #${inf.caseId}`,
    ],
    warnings,
  });
}
