// /achievements [member] (pkg-achievements.md §4.1): badges, tiers and progress bars, public like MEE6's.
import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { errorEmbed, infoEmbed, applyGuildColor, guildEmbed } from '../../utils/embedBuilder.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { safeDefer } from '../../utils/interactionHelpers.js';
import {
  loadMemberView, formatMemberEntry, badgeOf, TIER_EMOJIS, MEMBER_PAGE_SIZE,
} from '../../services/achievementService.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'achievements';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('achievements')
  .setDescription("Show a member's achievements, badges and progress (MEE6 /achievements)")
  .addUserOption((o) => o.setName('member').setDescription('Whose achievements to show (default: you)'));

export async function execute(interaction) {
  if (!(await safeDefer(interaction))) return;
  try {
    const user = interaction.options.getUser('member') ?? interaction.user;
    if (user.bot) {
      await interaction.editReply({ embeds: [errorEmbed('Not Available', "Bots don't earn achievements.")] });
      return;
    }
    const member = interaction.options.getMember('member') ?? (user.id === interaction.user.id ? interaction.member : null);
    const settings = await getSettingsSafe(interaction.guildId);
    const view = await loadMemberView(interaction.guildId, user.id, member);
    if (!view) {
      await interaction.editReply({ embeds: [errorEmbed('Something Went Wrong', 'I could not load achievements right now. Please try again in a moment.')] });
      return;
    }

    if (view.entries.length === 0) {
      const isAdmin = interaction.memberPermissions?.has(PermissionFlagsBits.ManageGuild);
      await interaction.editReply({
        embeds: [applyGuildColor(infoEmbed(
          'No Achievements',
          `This server has no achievements yet.${isAdmin ? ' Create one with `/achievement-config preset` or `/achievement-config create`.' : ''}`,
        ), settings)],
      });
      return;
    }

    // entries arrive ordered by createdAt, then id (loadMemberView reads them that way).
    const entries = view.entries;
    const displayName = member?.displayName ?? user.globalName ?? user.username;
    const totalTiers = entries.reduce((n, e) => n + e.a.tiers.length, 0);
    const unlockedTiers = entries.reduce((n, e) => n + e.display.tier, 0);
    const completed = entries.filter((e) => e.display.tier >= e.a.tiers.length).length;
    const badges = entries.filter((e) => e.display.tier >= 1).map((e) => `${badgeOf(e.a)}${TIER_EMOJIS[e.display.tier - 1]}`);
    let header = `**${unlockedTiers}/${totalTiers}** tiers unlocked · **${completed}/${entries.length}** completed`;
    if (badges.length) header += `\n**Badges:** ${joinWithLimit(badges, { sep: ' ', max: 300 })}`;

    const now = Date.now();
    const thumbnail = (member ?? user).displayAvatarURL({ extension: 'png', size: 128 });
    const pages = [];
    for (let i = 0; i < entries.length; i += MEMBER_PAGE_SIZE) {
      const body = entries.slice(i, i + MEMBER_PAGE_SIZE)
        .map((e) => formatMemberEntry(e.a, { ...e.display, enabled: e.enabled }, now))
        .join('\n\n');
      const embed = guildEmbed(settings, `🏆 ${clampText(displayName, 200)}'s achievements`, clampText(`${header}\n\n${body}`, 4096));
      if (thumbnail) embed.setThumbnail(thumbnail);
      pages.push(embed);
    }
    await paginate(interaction, pages);
  } catch (err) {
    logger.error('achievements failed', err);
    await interaction.editReply({ embeds: [errorEmbed('Something Went Wrong', 'I could not load achievements right now. Please try again in a moment.')] }).catch(() => {});
  }
}
