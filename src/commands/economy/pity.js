// /pity (pkg-economy.md §5.4, ours): PITY_AMOUNT coins when the balance is below PITY_THRESHOLD, once per 12 hours, per server.
// Refused while a casino game is running (the escrowed bet would make the balance look low).
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  PITY_AMOUNT, PITY_THRESHOLD, claimPity, formatCoins, isCasinoLocked, replyFail,
} from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { discordTimestamp } from '../../utils/time.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('pity')
  .setDescription(`Get ${PITY_AMOUNT} coins when your balance is below ${PITY_THRESHOLD} (12-hour cooldown).`);

export async function execute(interaction) {
  const { guildId, user } = interaction;
  if (isCasinoLocked(guildId, user.id)) {
    return interaction.reply({ embeds: [warnEmbed('Game In Progress', 'Finish your casino game first.')], ephemeral: true });
  }
  await interaction.deferReply();
  const settings = await getSettings(guildId);
  const r = await claimPity(guildId, user.id, settings);

  if (r.claimed) {
    return interaction.editReply({
      embeds: [successEmbed('💸 Pity Claimed', `You received ${formatCoins(PITY_AMOUNT, settings)} to get back in the game!`)
        .addFields({ name: 'Balance', value: formatCoins(r.balance, settings), inline: true })],
    });
  }
  if (r.tooRich) {
    return replyFail(interaction, null, null, {
      embed: infoEmbed('Pity Not Available', `Your balance is ${formatCoins(r.balance, settings)}. Pity is only for balances below ${formatCoins(PITY_THRESHOLD, settings)}.`),
    });
  }
  return replyFail(interaction, null, null, {
    embed: warnEmbed('Pity On Cooldown', `You can claim pity again ${discordTimestamp(r.nextAt, 'R')}.`),
  });
}
