// /work (pkg-economy.md §5.3, MEE6 /work): one atomic shift per workCooldownMinutes (default 60) that pays workMin..workMax.
// A qualifying coin boost applies once.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { claimWork, formatCoins, replyFail } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('work')
  .setDescription('Work a shift to earn coins (MEE6 /work).');

export async function execute(interaction) {
  await interaction.deferReply();
  const { guildId, user } = interaction;
  const settings = await getSettings(guildId);
  const r = await claimWork(guildId, user.id, settings, { memberRoleIds: [...(interaction.member?.roles?.cache?.keys() ?? [])] });

  if (!r.claimed) {
    return replyFail(interaction, null, null, {
      embed: warnEmbed('Still Tired', `You can work again ${discordTimestamp(r.nextAt, 'R')}.`),
    });
  }

  const boost = r.boost ? `\n🚀 Boost **${clampText(escapeMarkdown(r.boost.name), 40)}** +${r.boost.percent}%` : '';
  const embed = successEmbed('Shift Complete', `${r.line} and earned ${formatCoins(r.amount, settings)}!${boost}`).addFields(
    { name: 'Balance', value: formatCoins(r.balance, settings) + (r.capped ? ' (server maximum)' : ''), inline: true },
    { name: 'Next shift', value: discordTimestamp(r.nextAt, 'R'), inline: true },
  );
  return interaction.editReply({ embeds: [embed] });
}
