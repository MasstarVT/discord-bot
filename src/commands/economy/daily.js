// /daily (pkg-economy.md §5.2, MEE6 /daily): one atomic claim per 24 hours with a streak bonus (day 2 +10 %, day 3 +20 %,
// day 4 +40 %, day 5 +80 %, day 6 x2.5, day 7+ x4). The streak resets after 48 hours. A qualifying coin boost applies once.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { claimDaily, formatCoins, replyFail, streakMultiplierLabel } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('daily')
  .setDescription('Claim your daily coins; claim on consecutive days for a streak bonus (MEE6 /daily).');

export async function execute(interaction) {
  await interaction.deferReply();
  const { guildId, user } = interaction;
  const settings = await getSettings(guildId);
  const r = await claimDaily(guildId, user.id, settings, { memberRoleIds: [...(interaction.member?.roles?.cache?.keys() ?? [])] });

  if (!r.claimed) {
    const days = r.streak ? `\nCurrent streak: 🔥 ${r.streak} day${r.streak === 1 ? '' : 's'}.` : '';
    return replyFail(interaction, null, null, {
      embed: warnEmbed('Already Claimed', `You can claim your next daily reward ${discordTimestamp(r.nextAt, 'R')} (${discordTimestamp(r.nextAt, 't')}).${days}`),
    });
  }

  const boost = r.boost ? `\n🚀 Boost **${clampText(escapeMarkdown(r.boost.name), 40)}** +${r.boost.percent}%` : '';
  const footer = r.streak < 7
    ? `Tomorrow: ${streakMultiplierLabel(r.streak + 1)} · claim within 48 hours to keep your streak.`
    : 'Maximum streak bonus (×4) · claim within 48 hours to keep it.';
  const embed = successEmbed('Daily Reward Claimed', `You received ${formatCoins(r.amount, settings)}!${boost}`, footer).addFields(
    { name: 'Streak', value: `🔥 Day ${r.streak} (${streakMultiplierLabel(r.streak)})`, inline: true },
    { name: 'Balance', value: formatCoins(r.balance, settings) + (r.capped ? ' (server maximum)' : ''), inline: true },
    { name: 'Next claim', value: discordTimestamp(r.nextAt, 'R'), inline: true },
  );
  return interaction.editReply({ embeds: [embed] });
}
