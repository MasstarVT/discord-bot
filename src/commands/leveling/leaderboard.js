// /leaderboard (pkg-levels.md §5.4): the server XP leaderboard (MEE6 /levels). One query for the top 200 (xp desc, userId
// asc: the same tie rule as /rank), at most two gateway member requests for names, pages built in memory (R092, R093).
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import prisma from '../../database/client.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettings } from '../../services/settingsService.js';
import { LEADERBOARD_MAX_ROWS, getRank, levelFromXp } from '../../services/xpService.js';
import { withTimeout } from '../../utils/async.js';
import { guildEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'leveling';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const PAGE_SIZE = 10;
const MEMBER_CHUNK = 100;
const MAX_MEMBER_CHUNKS = 2;

export const data = new SlashCommandBuilder()
  .setName('leaderboard')
  .setDescription('Show the server XP leaderboard (MEE6 /levels).')
  .addIntegerOption((o) =>
    o.setName('page').setDescription('Page number').setMinValue(1).setMaxValue(20)
  );

export async function execute(interaction) {
  await interaction.deferReply();

  const { guild, guildId } = interaction;
  const settings = await getSettings(guildId);
  const total = await prisma.userLevel.count({ where: { guildId, xp: { gt: 0 } } });
  if (total === 0) return interaction.editReply({ embeds: [infoEmbed('Leaderboard', 'No one has earned XP in this server yet!')] });

  const rows = await prisma.userLevel.findMany({
    where: { guildId, xp: { gt: 0 } },
    orderBy: [{ xp: 'desc' }, { userId: 'asc' }],
    take: LEADERBOARD_MAX_ROWS,
    select: { userId: true, xp: true },
  });

  // Names without per-row REST calls: one gateway request per chunk of 100 members that are not cached yet.
  const missing = rows.map((r) => r.userId).filter((id) => !guild.members.cache.has(id));
  for (let i = 0; i < Math.min(missing.length, MEMBER_CHUNK * MAX_MEMBER_CHUNKS); i += MEMBER_CHUNK) {
    await withTimeout(guild.members.fetch({ user: missing.slice(i, i + MEMBER_CHUNK) }), 5000, 'leaderboard members').catch(() => null);
  }

  const lines = rows.map((row, i) => {
    const rank = i + 1;
    const member = guild.members.cache.get(row.userId);
    const name = member ? `**${escapeMarkdown(clampText(member.displayName, 32))}**` : `<@${row.userId}>`;
    const medal = rank === 1 ? '🥇' : rank === 2 ? '🥈' : rank === 3 ? '🥉' : `**#${rank}**`;
    return `${medal} ${name} — Level **${levelFromXp(row.xp)}** · ${formatNumber(row.xp)} XP`;
  });

  // The paginator appends " • Page X/N" to the footer itself (R244), so the footer carries no page text.
  const myIndex = rows.findIndex((r) => r.userId === interaction.user.id);
  let myRank = myIndex >= 0 ? myIndex + 1 : null;
  if (myRank === null) {
    const own = await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId, userId: interaction.user.id } }, select: { xp: true } });
    if (own && own.xp > 0) myRank = await getRank(guildId, interaction.user.id, own.xp);
  }
  const footer = `Top ${rows.length} of ${formatNumber(total)} ranked members${myRank ? ` · You: #${formatNumber(myRank)}` : ''}`;

  const pages = [];
  for (let i = 0; i < lines.length; i += PAGE_SIZE) {
    pages.push(
      guildEmbed(settings, clampText(`🏆 ${guild.name} Leaderboard`, 256), lines.slice(i, i + PAGE_SIZE).join('\n'), footer)
        .setThumbnail(guild.iconURL())
    );
  }
  const startPage = Math.min(Math.max((interaction.options.getInteger('page') ?? 1) - 1, 0), pages.length - 1);
  return paginate(interaction, pages, { startPage });
}
