import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS, xpForLevel } from '../../config/constants.js';
import { levelFromXp, xpIntoLevel } from '../../services/xpService.js';
import { paginate } from '../../utils/paginator.js';
import prisma from '../../database/client.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const PAGE_SIZE  = 10;
const MAX_PAGES  = 20; // show top 200 users maximum

export const data = new SlashCommandBuilder()
  .setName('leaderboard')
  .setDescription('View the XP leaderboard for this server.')
  .addIntegerOption((o) =>
    o.setName('page').setDescription('Page number').setMinValue(1)
  );

export async function execute(interaction, client) {
  await interaction.deferReply();

  const guild   = interaction.guild;
  const guildId = guild.id;

  const totalUsers = await prisma.userLevel.count({ where: { guildId } });
  if (totalUsers === 0) {
    return interaction.editReply({ content: 'No one has earned XP in this server yet!' });
  }

  const totalPages = Math.min(Math.ceil(totalUsers / PAGE_SIZE), MAX_PAGES);
  const startPage  = Math.min(Math.max((interaction.options.getInteger('page') ?? 1) - 1, 0), totalPages - 1);

  const buildPage = async (pageIndex) => {
    const rows = await prisma.userLevel.findMany({
      where:   { guildId },
      orderBy: { xp: 'desc' },
      skip:    pageIndex * PAGE_SIZE,
      take:    PAGE_SIZE,
    });

    const lines = await Promise.all(
      rows.map(async (row, i) => {
        const globalRank = pageIndex * PAGE_SIZE + i + 1;
        const level      = levelFromXp(row.xp);
        let displayName  = `<@${row.userId}>`;

        try {
          const member = guild.members.cache.get(row.userId)
            ?? await guild.members.fetch(row.userId).catch(() => null);
          if (member) displayName = member.displayName;
        } catch { /* use mention */ }

        const medal = globalRank === 1 ? '🥇' : globalRank === 2 ? '🥈' : globalRank === 3 ? '🥉' : `**#${globalRank}**`;

        return `${medal} ${displayName} — Level **${level}** · ${row.xp.toLocaleString()} XP`;
      })
    );

    return new EmbedBuilder()
      .setColor(COLORS.INFO)
      .setTitle(`🏆 ${guild.name} Leaderboard`)
      .setDescription(lines.join('\n'))
      .setThumbnail(guild.iconURL())
      .setFooter({ text: `Page ${pageIndex + 1} of ${totalPages} · Top ${Math.min(totalUsers, MAX_PAGES * PAGE_SIZE)} of ${totalUsers} members` })
      .setTimestamp();
  };

  // Pre-build all pages lazily — paginator receives an array of embed-builders or built embeds.
  // For large servers, build pages on-demand using the paginator callback variant.
  const pages = [];
  for (let i = 0; i < totalPages; i++) {
    pages.push(await buildPage(i));
  }

  await paginate(interaction, pages, { startPage });
}
