import { randomInt } from 'node:crypto';
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { applyGuildColor, errorEmbed, infoEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone, missingBotPermissions, replyError } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

/** userId → true while the user has a game in progress. */
const activeGames = new Set();

export const MAX_GUESSES = 7;
export const GUESS_IDLE_MS = 60_000;     // disclosed in the intro; reset by every guess (R234)
export const GAME_MAX_MS = 10 * 60_000;  // hard cap, below the 15-minute interaction token lifetime
const WARN_EVERY_MS = 5_000;

export const data = new SlashCommandBuilder()
  .setName('numguess')
  .setDescription(`Guess the bot's number (1–100) in ${MAX_GUESSES} tries!`);

/**
 * @param {number} secret
 * @param {number} guess
 * @returns {'out-of-range'|'correct'|'higher'|'lower'} 'higher' means the secret is higher than the guess
 */
export function evaluateGuess(secret, guess) {
  if (!Number.isInteger(guess) || guess < 1 || guess > 100) return 'out-of-range';
  if (guess === secret) return 'correct';
  return secret > guess ? 'higher' : 'lower';
}

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'numguess settings').catch(() => null);

export async function execute(interaction) {
  const userId = interaction.user.id;
  const channel = interaction.channel;

  if (!channel || !channel.isTextBased()) {
    return replyError(interaction, 'Unavailable Here', "I can't read messages in this channel, so I can't play here.");
  }
  // R080: the only channel permission the game needs. All bot output goes through the interaction
  // webhook (followUp), so Send Messages and Read Message History are not required.
  if (missingBotPermissions(channel, ['ViewChannel']).length > 0) {
    return replyError(interaction, 'Missing Permissions', 'I need **View Channel** here to read your guesses. Try another channel or ask an admin.');
  }
  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active guessing game!', ephemeral: true });
  }

  const secret = randomInt(1, 101);
  const settings = await loadSettings(interaction);
  if (activeGames.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active guessing game!', ephemeral: true });
  }
  activeGames.add(userId);

  // R089: the collector's `end` releases the lock; release it here if anything throws before it exists.
  let handedOff = false;
  let collector = null;
  try {
    await interaction.reply({
      embeds: [applyGuildColor(infoEmbed(
        'Number Guessing Game',
        `I'm thinking of a number between **1** and **100**.\nYou have **${MAX_GUESSES}** guesses — type them in this channel.\n`
          + 'The game ends if you don\'t guess for **60 seconds** (10 minutes max).',
      ), settings)],
    });

    // Only numeric messages of the player; typed messages are the only input, so this stays a channel collector.
    collector = channel.createMessageCollector({
      filter: (m) => m.author.id === userId && /^\s*\d{1,6}\s*$/.test(m.content),
      idle: GUESS_IDLE_MS,
      time: GAME_MAX_MS,
    });

    // Feedback goes through the interaction webhook: no channel permissions, and it survives a deleted guess.
    const say = (embed) => interaction.followUp({ embeds: [embed] }).catch((err) => {
      if (isInteractionGone(err)) logger.debug('numguess: interaction gone', err);
      else logger.warn('numguess follow-up failed', err);
    });

    let guessCount = 0;
    let finished = false;
    let lastWarnAt = 0;

    collector.on('collect', async (msg) => {
      try {
        if (finished) return; // R235
        const guess = parseInt(msg.content.trim(), 10);
        const verdict = evaluateGuess(secret, guess);

        if (verdict === 'out-of-range') {
          // Doesn't count against the limit; answered at most once per 5 s so spam can't flood the webhook.
          const now = Date.now();
          if (now - lastWarnAt < WARN_EVERY_MS) return;
          lastWarnAt = now;
          await say(warnEmbed('Out of Range', 'Please guess a number between **1** and **100**.'));
          return;
        }

        guessCount++;
        const remaining = MAX_GUESSES - guessCount;

        if (verdict === 'correct') {
          finished = true;
          collector.stop('won'); // before any await (R080, R235)
          await say(successEmbed('Correct!', `The number was **${secret}**! You got it in **${guessCount}/${MAX_GUESSES}** guesses. 🎉`));
          return;
        }
        if (guessCount >= MAX_GUESSES) {
          finished = true;
          collector.stop('exhausted');
          await say(errorEmbed('Out of Guesses!', `The number was **${secret}**. Better luck next time!`));
          return;
        }
        await say(applyGuildColor(infoEmbed(
          verdict === 'higher' ? '📈 Higher!' : '📉 Lower!',
          `**${guess}** is wrong. ${remaining} guess${remaining !== 1 ? 'es' : ''} remaining · 60 s to guess.`,
        ), settings));
      } catch (err) {
        logger.error('Number guess collect failed', err);
      }
    });

    collector.on('end', async (collected, reason) => {
      try {
        activeGames.delete(userId);
        if (reason === 'won' || reason === 'exhausted') return;
        if (reason === 'idle') {
          // No collected message at all usually means the bot can't see messages here.
          const hint = collected.size === 0 ? '\nIf you did type numbers, I may not be able to read messages in this channel.' : '';
          await say(errorEmbed("Time's Up!", `You didn't guess for 60 seconds. The number was **${secret}**.${hint}`));
        } else if (reason === 'time') {
          await say(errorEmbed("Time's Up!", `The game reached its 10-minute limit. The number was **${secret}**.`));
        }
        // Anything else (channelDelete, threadDelete, guildDelete, user): no message.
      } catch (err) {
        logger.error('Number guess end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) {
      activeGames.delete(userId);
      collector?.stop('error');
    }
  }
}
