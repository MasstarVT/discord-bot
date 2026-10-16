// /rank (pkg-levels.md §5.3): the rank card of a member (MEE6 /rank). Uses the member's server name and avatar (R245),
// the leaderboard's tie rule for the position (R243) and the server's rank-card style.
import { AttachmentBuilder, SlashCommandBuilder } from 'discord.js';
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS, xpForLevel } from '../../config/constants.js';
import { getSettings } from '../../services/settingsService.js';
import { getRank, levelFromXp, xpIntoLevel } from '../../services/xpService.js';
import { guildEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { generateProfileCard, loadCardBackground, normalizeCardStyle } from '../../utils/profileCard.js';
import { safeFetchImage } from '../../utils/safeFetch.js';
import { formatNumber } from '../../utils/text.js';

export const module = 'leveling';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('rank')
  .setDescription('Show your rank card or a member\'s (MEE6 /rank).')
  .addUserOption((o) =>
    o.setName('user').setDescription('Member to look up (default: you)')
  );

export async function execute(interaction) {
  await interaction.deferReply();

  const { guild, guildId } = interaction;
  const user = interaction.options.getUser('user') ?? interaction.user;
  const member = interaction.options.getMember('user') ?? (user.id === interaction.user.id ? interaction.member : null);
  const name = member?.displayName ?? user.globalName ?? user.username;

  if (user.bot) return interaction.editReply({ embeds: [infoEmbed('No Rank', "Bots don't earn XP.")] });

  // Independent waits overlap: the avatar download starts at once, the row and settings load together, then the
  // position and background load together. Sequential, they added up to five round trips.
  const avatarUrl = (member ?? user).displayAvatarURL({ extension: 'png', size: 256 });
  const avatarPromise = safeFetchImage(avatarUrl, { maxBytes: 2 * 1024 * 1024, timeoutMs: 5000 }).catch(() => null);

  const [record, settings] = await Promise.all([
    prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: user.id } } }),
    getSettings(guildId),
  ]);
  if (!record || record.xp <= 0) {
    return interaction.editReply({ embeds: [infoEmbed('No XP Yet', `${name} hasn't earned any XP in this server yet.`)] });
  }

  const level = levelFromXp(record.xp);
  const xpCurrent = xpIntoLevel(record.xp);
  const xpNeeded = xpForLevel(level);
  const style = normalizeCardStyle(settings.rankCardStyle);
  const [rank, avatarBuffer, backgroundBuffer] = await Promise.all([
    getRank(guildId, user.id, record.xp),
    avatarPromise,
    style.backgroundUrl ? loadCardBackground(style.backgroundUrl) : null,
  ]);
  const card = await generateProfileCard({
    avatarBuffer,
    username: user.username,
    displayName: name,
    level,
    totalXp: record.xp,
    rank,
    xpCurrent,
    xpNeeded,
    style,
    backgroundBuffer,
  }).catch((err) => {
    logger.warn(`rank: could not draw the card for ${user.id} in ${guildId}: ${err?.message ?? err}`);
    return null;
  });

  if (card) {
    try {
      return await interaction.editReply({ files: [new AttachmentBuilder(card, { name: 'rank.png' })] });
    } catch (err) {
      if (isInteractionGone(err)) return undefined;
      logger.warn(`rank: could not send the card: ${err?.message ?? err}`);
    }
  }

  // Fallback: an embed with the same numbers.
  const embed = guildEmbed(settings, `${name}'s Rank`)
    .addFields(
      { name: 'Level', value: formatNumber(level), inline: true },
      { name: 'Rank', value: `#${formatNumber(rank)}`, inline: true },
      { name: 'Total XP', value: formatNumber(record.xp), inline: true },
      { name: 'Progress', value: `${formatNumber(xpCurrent)} / ${formatNumber(xpNeeded)} XP`, inline: true },
      { name: 'Messages', value: formatNumber(record.messageCount), inline: true },
      { name: 'Voice', value: `${formatNumber(record.voiceMinutes)} min`, inline: true },
    )
    .setThumbnail(avatarUrl)
    .setFooter({ text: guild.name });
  return interaction.editReply({ embeds: [embed] });
}
