// /pay (pkg-economy.md §5.5, ours; MEE6 /give-coins between members): moves coins from you to another member in one
// transaction. Coins are never created or destroyed: a recipient at the server maximum refuses the transfer.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { COIN_MAX, PERMISSION_LEVELS } from '../../config/constants.js';
import { economyConfig, formatCoins, replyFail, transfer } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { errorEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { clampText } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('pay')
  .setDescription('Give some of your coins to another member (MEE6 /give-coins).')
  .addUserOption((o) => o.setName('member').setDescription('Who receives the coins').setRequired(true))
  .addIntegerOption((o) => o.setName('amount').setDescription('How many coins to give').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX));

const privateError = (interaction, title, description) =>
  interaction.reply({ embeds: [errorEmbed(title, description)], ephemeral: true });

export async function execute(interaction) {
  const { guildId, user } = interaction;
  const target = interaction.options.getUser('member', true);
  const amount = interaction.options.getInteger('amount', true);
  // Checks that need no database run before the deferral and answer privately.
  if (target.id === user.id) return privateError(interaction, 'Not Allowed', "You can't pay yourself.");
  if (target.bot) return privateError(interaction, 'Not Allowed', "Bots can't hold coins or items.");
  if (!interaction.options.getMember('member')) return privateError(interaction, 'Not A Member', 'That user is not a member of this server.');

  await interaction.deferReply();
  const settings = await getSettings(guildId);
  const cfg = economyConfig(settings);
  const r = await transfer(guildId, user.id, target.id, amount, settings);

  if (!r.ok && r.code === 'INSUFFICIENT') {
    return replyFail(interaction, 'Insufficient Funds', `You only have ${formatCoins(r.balance, settings)}.`);
  }
  if (!r.ok) {
    return replyFail(interaction, 'Balance Limit',
      `${clampText(escapeMarkdown(target.username), 100)} can't hold that many ${cfg.currencyName} (the maximum is ${formatCoins(cfg.maxBalance, settings)}).`);
  }
  const giver = clampText(escapeMarkdown(interaction.member?.displayName ?? user.username), 100);
  return interaction.editReply({
    embeds: [successEmbed('Coins Sent', `${giver} gave ${formatCoins(amount, settings)} to ${target}.`).addFields(
      { name: 'Your balance', value: formatCoins(r.fromBalance, settings), inline: true },
      { name: 'Their balance', value: formatCoins(r.toBalance, settings), inline: true },
    )],
  });
}
