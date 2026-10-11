// /richest (pkg-economy.md §5.6, MEE6 /richest): the top 100 balances of this server, 10 per page, plus the caller's own rank.
// Viewing never creates rows (peekAccount) and members are mentioned, not fetched.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { economyConfig, formatCoins, getRank, peekAccount, topAccounts } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { guildEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const PER_PAGE = 10;
const MEDALS = ['🥇', '🥈', '🥉'];

export const data = new SlashCommandBuilder()
  .setName('richest')
  .setDescription('Show the richest members of this server (MEE6 /richest).');

export async function execute(interaction) {
  await interaction.deferReply();
  const { guildId, user } = interaction;
  const settings = await getSettings(guildId);
  const rows = await topAccounts(guildId);

  if (rows.length === 0) {
    return interaction.editReply({
      embeds: [guildEmbed(settings, '🏆 Richest Members', `Nobody has any ${economyConfig(settings).currencyName} yet. Try /daily!`)],
    });
  }

  const lines = rows.map((row, i) => `${i < 3 ? MEDALS[i] : `**#${i + 1}**`} <@${row.userId}> — ${formatCoins(row.balance, settings)}`);
  const acc = await peekAccount(guildId, user.id, settings);
  const footer = acc.exists && acc.balance > 0
    ? `Your rank: #${formatNumber(await getRank(guildId, acc.balance))} · ${formatCoins(acc.balance, settings, { bold: false })}`
    : undefined;

  const pages = [];
  for (let i = 0; i < lines.length; i += PER_PAGE) {
    pages.push(guildEmbed(settings, '🏆 Richest Members', lines.slice(i, i + PER_PAGE).join('\n'), footer));
  }
  return paginate(interaction, pages, { footer: 'append' });
}
