// /balance (pkg-economy.md §5.1, MEE6 /coins): a member's coins on THIS server. Looking at yourself creates the account
// (lazy global copy); looking at someone else never creates rows (peekAccount).
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { activeStreak, formatCoins, getAccount, getRank, peekAccount } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('balance')
  .setDescription("Show your or a member's coins on this server (MEE6 /coins).")
  .addUserOption((o) => o.setName('user').setDescription('Member to check (default: you)'));

export async function execute(interaction) {
  const target = interaction.options.getUser('user') ?? interaction.user;
  if (target.bot) {
    return interaction.reply({ embeds: [errorEmbed('Not Allowed', "Bots can't hold coins or items.")], ephemeral: true });
  }
  await interaction.deferReply();

  const { guildId } = interaction;
  const settings = await getSettings(guildId);
  const self = target.id === interaction.user.id;
  const acc = self
    ? { ...(await getAccount(guildId, target.id, settings)), exists: true }
    : await peekAccount(guildId, target.id, settings);
  const name = clampText(escapeMarkdown(
    (self ? interaction.member?.displayName : interaction.options.getMember('user')?.displayName) ?? target.displayName ?? target.username,
  ), 100);

  const embed = guildEmbed(settings, `💰 ${name}'s balance`, formatCoins(acc.balance, settings), self ? 'Earn more with /daily and /work.' : undefined);
  if (acc.exists && acc.balance > 0) {
    embed.addFields({ name: 'Rank', value: `#${formatNumber(await getRank(guildId, acc.balance))}`, inline: true });
  }
  const streak = activeStreak(acc.lastDaily, acc.dailyStreak);
  if (streak > 0) embed.addFields({ name: 'Daily streak', value: `🔥 ${streak} day${streak === 1 ? '' : 's'}`, inline: true });
  return interaction.editReply({ embeds: [embed] });
}
