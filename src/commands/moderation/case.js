import { PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { COLORS, PERMISSION_LEVELS } from '../../config/constants.js';
import {
  ACTION_META,
  MSG,
  deliverLookup,
  displayType,
  postModLog,
} from '../../services/moderationService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed, neutralEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import logger from '../../utils/logger.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;

const MAX_CASE_ID = 2_147_483_647; // infraction.caseId is a 32-bit Int column
const NO_MENTIONS = { parse: [] };

export const data = new SlashCommandBuilder()
  .setName('case')
  .setDescription('View or edit a moderation case.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addSubcommand((s) => s.setName('view').setDescription('View a case.')
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1)))
  .addSubcommand((s) => s.setName('reason').setDescription("Change a case's reason.")
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1))
    .addStringOption((o) => o.setName('reason').setDescription('New reason').setRequired(true).setMaxLength(1000)))
  .addSubcommand((s) => s.setName('proof').setDescription("Set a case's proof link.")
    .addIntegerOption((o) => o.setName('id').setDescription('Case number').setRequired(true).setMinValue(1))
    .addStringOption((o) => o.setName('url').setDescription('Proof link (http/https)').setRequired(true).setMaxLength(1000)));

const notFound = (caseId) => errorEmbed('Not Found', `Case #${caseId} doesn't exist in this server.`);
const quote = (text) => String(text).replace(/\n/g, '\n> ');

/** "<@id> `tag`", or "Unknown" when the user cannot be loaded. */
async function personField(client, id) {
  const user = client.users.cache.get(id) ?? await client.users.fetch(id).catch(() => null);
  if (!user) return `<@${id}> \`Unknown\``;
  return `<@${id}> \`${clampText(String(user.tag ?? user.username ?? id).replace(/`/g, "'"), 100)}\``;
}

/** Best-effort note in the case-log channel; postModLog never throws, the catch is a second guard. */
async function logEdit(guild, caseId, text) {
  try {
    await postModLog(guild, neutralEmbed(`📝 Case #${caseId} edited`, clampText(text, 4000)));
  } catch (err) {
    logger.warn(`/case: mod log note failed: ${err?.message ?? err}`);
  }
}

/**
 * /case view | reason | proof. Every displayed value is clamped (older rows may hold very long text),
 * and reason / proof edits are bounded to 1000 characters (R095).
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {import('discord.js').Client} [client]
 * @returns {Promise<void>}
 */
export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true }); // errors stay private

  const sub = interaction.options.getSubcommand();
  const caseId = interaction.options.getInteger('id', true);
  const guild = interaction.guild;
  const guildId = interaction.guildId;
  const where = { guildId_caseId: { guildId, caseId } };

  if (caseId < 1 || caseId > MAX_CASE_ID) {
    await interaction.editReply({ embeds: [notFound(caseId)] });
    return;
  }

  // ── view ──────────────────────────────────────────────────────────────────
  if (sub === 'view') {
    const settings = await getSettingsSafe(guildId);
    const inf = await prisma.infraction.findUnique({ where });
    if (!inf) {
      await interaction.editReply({ embeds: [notFound(caseId)] });
      return;
    }

    const bot = client ?? interaction.client;
    const [target, moderator] = await Promise.all([personField(bot, inf.targetUserId), personField(bot, inf.moderatorId)]);
    const meta = ACTION_META[displayType(inf)] ?? { emoji: '📋', label: String(inf.type), color: COLORS.NEUTRAL };
    const embed = neutralEmbed(clampText(`${meta.emoji} Case #${caseId} | ${meta.label}`, 256)).setColor(meta.color);
    embed.addFields(
      { name: 'Target', value: target, inline: true },
      { name: 'Moderator', value: moderator, inline: true },
      { name: 'Reason', value: clampText(inf.reason || 'None', 1024) },
      { name: 'Active', value: inf.active ? 'Yes' : 'No', inline: true },
      { name: 'Created', value: discordTimestamp(inf.createdAt, 'F'), inline: true },
    );
    if (inf.expiresAt) {
      embed.addFields({ name: 'Expires', value: `${discordTimestamp(inf.expiresAt, 'F')} (${discordTimestamp(inf.expiresAt, 'R')})`, inline: true });
    }
    if (inf.proofUrl) embed.addFields({ name: 'Proof', value: clampText(inf.proofUrl, 1024) });
    embed.setFooter({ text: `User ID: ${inf.targetUserId}` });
    await deliverLookup(interaction, settings, { embeds: [embed], allowedMentions: NO_MENTIONS });
    return;
  }

  // ── reason / proof ────────────────────────────────────────────────────────
  let data;
  let confirmation;
  let logText;
  if (sub === 'reason') {
    const newReason = clampText(interaction.options.getString('reason', true).trim(), 1000);
    if (!newReason) {
      await interaction.editReply({ embeds: [errorEmbed('Invalid Input', "The reason can't be empty.")] });
      return;
    }
    data = { reason: newReason };
    confirmation = successEmbed('Case Updated', `Case #${caseId} reason is now:\n> ${quote(clampText(newReason, 1000))}`);
    logText = `Reason changed by <@${interaction.user.id}>:\n> ${quote(newReason)}`;
  } else {
    const url = interaction.options.getString('url', true).trim();
    if (!isHttpUrl(url) || url.length > 1000) {
      await interaction.editReply({ embeds: [errorEmbed('Invalid Link', MSG.BAD_PROOF)] });
      return;
    }
    data = { proofUrl: url };
    confirmation = successEmbed('Case Updated', `Case #${caseId} proof link updated.`);
    logText = `Proof link changed by <@${interaction.user.id}>:\n${url}`;
  }

  const settings = await getSettingsSafe(guildId);
  try {
    await prisma.infraction.update({ where, data });
  } catch (err) {
    if (err?.code === 'P2025') {
      await interaction.editReply({ embeds: [notFound(caseId)] });
      return;
    }
    throw err;
  }
  await deliverLookup(interaction, settings, { embeds: [confirmation], allowedMentions: NO_MENTIONS });
  await logEdit(guild, caseId, logText);
}
