import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { errorEmbed } from '../../utils/embedBuilder.js';

export const module = null; // core command, never gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('owner')
  .setDescription('Show who owns this bot.');

export async function execute(interaction) {
  const ownerId = process.env.BOT_OWNER_ID?.trim();
  if (!/^\d{17,20}$/.test(ownerId ?? '')) {
    return interaction.reply({
      embeds: [errorEmbed('No Owner Set', 'This bot has no owner configured (`BOT_OWNER_ID`).')],
      ephemeral: true,
    });
  }

  // Mentions inside embeds never ping, so the owner goes in plain content and only that user may be pinged.
  return interaction.reply({
    content: `👑 <@${ownerId}> is the owner of this bot.`,
    allowedMentions: { users: [ownerId] },
  });
}
