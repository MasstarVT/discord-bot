import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { claimDaily, DAILY_AMOUNT } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('daily')
  .setDescription(`Claim your daily ${DAILY_AMOUNT} credits (resets every 24 hours).`);

export async function execute(interaction) {
  const result = await claimDaily(interaction.user.id);

  if (!result.claimed) {
    const remaining = result.nextAt - Date.now();
    const hours     = Math.floor(remaining / 3_600_000);
    const minutes   = Math.floor((remaining % 3_600_000) / 60_000);
    return interaction.reply({
      embeds: [warnEmbed('Already Claimed', `You already claimed your daily reward. Come back in **${hours}h ${minutes}m**.`)],
      ephemeral: true,
    });
  }

  await interaction.reply({
    embeds: [successEmbed('Daily Reward Claimed!', `You received **${DAILY_AMOUNT}** credits!\nNew balance: **${result.balance.toLocaleString()}** credits`)],
  });
}
