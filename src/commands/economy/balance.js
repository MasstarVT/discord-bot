import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { infoEmbed } from '../../utils/embedBuilder.js';
import { getOrCreate } from '../../services/economyService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('balance')
  .setDescription('Check your credit balance.')
  .addUserOption((o) =>
    o.setName('user').setDescription('User to check (defaults to yourself)').setRequired(false)
  );

export async function execute(interaction) {
  const target  = interaction.options.getUser('user') ?? interaction.user;
  const account = await getOrCreate(target.id);
  const isSelf  = target.id === interaction.user.id;

  await interaction.reply({
    embeds: [infoEmbed(
      `💰 ${isSelf ? 'Your Balance' : `${target.username}'s Balance`}`,
      `**${account.balance.toLocaleString()}** credits`,
    )],
  });
}
