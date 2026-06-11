import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, warnEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { claimPity, PITY_THRESHOLD, PITY_AMOUNT } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('pity')
  .setDescription(`Claim ${PITY_AMOUNT} free credits if your balance drops below ${PITY_THRESHOLD} (12-hour cooldown).`);

export async function execute(interaction) {
  const result = await claimPity(interaction.user.id);

  if (result.tooRich) {
    return interaction.reply({
      embeds: [infoEmbed(
        'Pity Not Available',
        `Your balance is **${result.balance}** credits — pity is only available when you drop below **${PITY_THRESHOLD}** credits.`,
      )],
      ephemeral: true,
    });
  }

  if (!result.claimed) {
    const remaining = result.nextAt - Date.now();
    const hours     = Math.floor(remaining / 3_600_000);
    const minutes   = Math.floor((remaining % 3_600_000) / 60_000);
    return interaction.reply({
      embeds: [warnEmbed('Pity On Cooldown', `You already claimed pity recently. Come back in **${hours}h ${minutes}m**.`)],
      ephemeral: true,
    });
  }

  await interaction.reply({
    embeds: [successEmbed(
      '💸 Pity Claimed!',
      `You received **${PITY_AMOUNT}** credits to get back in the game!\nNew balance: **${result.balance.toLocaleString()}** credits`,
    )],
  });
}
