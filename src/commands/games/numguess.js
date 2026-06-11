import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { infoEmbed, successEmbed, errorEmbed, warnEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const activeGames = new Set();
const MAX_GUESSES = 7;

export const data = new SlashCommandBuilder()
  .setName('numguess')
  .setDescription(`Guess the bot's number (1–100) in ${MAX_GUESSES} tries!`);

export async function execute(interaction) {
  const userId = interaction.user.id;

  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active guessing game!', ephemeral: true });
  }
  activeGames.add(userId);

  const secret = Math.floor(Math.random() * 100) + 1;
  let guessCount = 0;

  await interaction.reply({
    embeds: [infoEmbed(
      'Number Guessing Game',
      `I'm thinking of a number between **1** and **100**.\nYou have **${MAX_GUESSES}** guesses — type them in this channel!`,
    )],
  });

  // Only collect numeric messages from the initiating user
  const filter    = (msg) => msg.author.id === userId && /^\d+$/.test(msg.content.trim());
  const collector = interaction.channel.createMessageCollector({ filter, time: 120_000 });

  collector.on('collect', async (msg) => {
    const guess = parseInt(msg.content.trim(), 10);

    if (guess < 1 || guess > 100) {
      await msg.reply({ embeds: [warnEmbed('Out of Range', 'Please guess a number between **1** and **100**.')] });
      return; // don't count out-of-range inputs against the limit
    }

    guessCount++;
    const remaining = MAX_GUESSES - guessCount;

    if (guess === secret) {
      await msg.reply({ embeds: [successEmbed('Correct!', `The number was **${secret}**! You got it in **${guessCount}/${MAX_GUESSES}** guesses. 🎉`)] });
      collector.stop('won');
      return;
    }

    if (guessCount >= MAX_GUESSES) {
      await msg.reply({ embeds: [errorEmbed('Out of Guesses!', `The number was **${secret}**. Better luck next time!`)] });
      collector.stop('exhausted');
      return;
    }

    const hint = guess < secret ? '📈 Higher!' : '📉 Lower!';
    await msg.reply({
      embeds: [infoEmbed(hint, `**${guess}** is wrong. ${remaining} guess${remaining !== 1 ? 'es' : ''} remaining.`)],
    });
  });

  collector.on('end', (_, reason) => {
    activeGames.delete(userId);
    if (reason === 'won' || reason === 'exhausted') return;
    // Timed out before the player finished
    interaction.followUp({
      embeds: [errorEmbed("⏰ Time's Up!", `You ran out of time! The number was **${secret}**.`)],
    }).catch(() => {});
  });
}
