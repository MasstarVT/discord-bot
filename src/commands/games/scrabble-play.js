import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  COLS, currentPlayerId, describeWords, exchangeTiles, formatRack, gameMutex, getGame, passTurn,
  playWord, postBoard, postResult, resignGame,
} from '../../services/scrabbleService.js';
import { errorEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm, isInteractionGone, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const COL_CHOICES = 'ABCDEFGHIJKLMNO'.split('').map((c) => ({ name: c, value: c }));

export const data = new SlashCommandBuilder()
  .setName('scrabble-play')
  .setDescription('Scrabble move: word, pass, exchange or resign')
  .addSubcommand((s) => s.setName('word').setDescription('Place a word')
    .addStringOption((o) => o.setName('word').setDescription('Letters; ?X = blank played as X')
      .setRequired(true).setMinLength(1).setMaxLength(17))
    .addIntegerOption((o) => o.setName('row').setDescription('Row 1-15')
      .setRequired(true).setMinValue(1).setMaxValue(15))
    .addStringOption((o) => o.setName('col').setDescription('Column A-O')
      .setRequired(true).addChoices(...COL_CHOICES))
    .addStringOption((o) => o.setName('direction').setDescription('Direction')
      .setRequired(true).addChoices({ name: 'Horizontal', value: 'h' }, { name: 'Vertical', value: 'v' })))
  .addSubcommand((s) => s.setName('pass').setDescription('Skip your turn'))
  .addSubcommand((s) => s.setName('exchange').setDescription('Swap tiles with the bag')
    .addStringOption((o) => o.setName('tiles').setDescription('Letters to swap; ? = blank')
      .setRequired(true).setMinLength(1).setMaxLength(7)))
  .addSubcommand((s) => s.setName('resign').setDescription('Forfeit the game'));

const NO_GAME = () => errorEmbed('No Active Game', 'There is no Scrabble game running in this channel. Start one with `/scrabble`.');
const NOT_PLAYER = () => errorEmbed('Not Your Game', "You're not a player in this game.");

/** Ephemeral edit of the deferred reply; the move is already committed, so a failure is only logged. */
async function tell(interaction, payload) {
  try {
    await interaction.editReply(payload);
  } catch (err) {
    if (isInteractionGone(err)) logger.debug('scrabble-play: interaction gone', err);
    else logger.warn('scrabble-play: reply failed', err);
  }
}

/**
 * Posts the public follow-up after a committed move: the next board (with a real mention of the next
 * player, R084) or the final result. The ephemeral reply is edited FIRST, because the first follow-up
 * after a deferred reply would otherwise become that reply and stay private.
 */
async function announce(game, interaction, outcome, text) {
  if (outcome.gameOver) {
    await postResult(game, interaction, outcome.gameOver);
    return;
  }
  const next = currentPlayerId(game);
  await postBoard(game, interaction, {
    content: `<@${next}>, it's your turn! ${text}`,
    mentionIds: [next],
  });
}

async function handleMove(interaction, sub) {
  const channelId = interaction.channelId;
  const userId = interaction.user.id;

  const game = getGame(channelId);
  if (!game) return tell(interaction, { embeds: [NO_GAME()] });
  const me = game.players[userId];
  if (!me) return tell(interaction, { embeds: [NOT_PLAYER()] });

  // The service calls below are synchronous (validate + commit with no await in between); the mutex
  // only keeps the Discord calls of consecutive moves in order.
  if (sub === 'word') {
    const col0 = COLS.indexOf(interaction.options.getString('col'));
    const r = playWord(game, userId, interaction.options.getString('word'), interaction.options.getInteger('row') - 1, col0, interaction.options.getString('direction'));
    if (!r.ok) return tell(interaction, { embeds: [errorEmbed('Invalid Move', r.error)] });
    await tell(interaction, {
      embeds: [successEmbed('Word Played!',
        `${describeWords(r.words)}${r.bingo ? ' + **50** bingo 🎉' : ''} = **${r.total}** pts.\n🎒 Your tiles: ${formatRack(me.rack)}`)],
    });
    await announce(game, interaction, r, `**${me.name}** played ${describeWords(r.words)} for **${r.total}** points.`);
    return;
  }

  if (sub === 'pass') {
    const r = passTurn(game, userId);
    if (!r.ok) return tell(interaction, { embeds: [errorEmbed('Invalid Move', r.error)] });
    await tell(interaction, { embeds: [successEmbed('Turn Passed', 'You passed your turn.')] });
    await announce(game, interaction, r, `**${me.name}** passed.`);
    return;
  }

  if (sub === 'exchange') {
    const r = exchangeTiles(game, userId, interaction.options.getString('tiles'));
    if (!r.ok) return tell(interaction, { embeds: [errorEmbed('Invalid Move', r.error)] });
    await tell(interaction, {
      embeds: [successEmbed('Tiles Exchanged',
        `You swapped **${r.count}** tile${r.count === 1 ? '' : 's'}.\n🎒 Your tiles: ${formatRack(me.rack)}`)],
    });
    // Never reveal which tiles were swapped.
    await announce(game, interaction, r, `**${me.name}** exchanged ${r.count} tile${r.count === 1 ? '' : 's'}.`);
  }
}

async function handleResign(interaction) {
  const channelId = interaction.channelId;
  const userId = interaction.user.id;

  const game = getGame(channelId);
  if (!game) return tell(interaction, { embeds: [NO_GAME()] });
  if (!game.players[userId]) return tell(interaction, { embeds: [NOT_PLAYER()] });
  const gameId = game.id;

  // Outside the mutex, so other moves are not blocked while the player decides.
  const ok = await awaitConfirm(interaction, {
    title: 'Resign this game?',
    description: 'Your opponent will be declared the winner.',
    confirmLabel: 'Resign',
    cancelLabel: 'Keep playing',
    danger: true,
    timeoutMs: 30_000,
    ephemeral: true,
  });
  if (!ok) return;

  await gameMutex.run(channelId, async () => {
    const g = getGame(channelId);
    if (!g || g.id !== gameId) {
      await safeReply(interaction, { embeds: [errorEmbed('No Active Game', 'That game has already ended.')], ephemeral: true });
      return;
    }
    const result = resignGame(g, userId);
    await postResult(g, interaction, result);
  });
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  // R083: acknowledge first, then serialise every move of the channel.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  try {
    if (sub === 'resign') {
      await handleResign(interaction);
      return;
    }
    await gameMutex.run(interaction.channelId, () => handleMove(interaction, sub));
  } catch (err) {
    if (isInteractionGone(err)) logger.debug('scrabble-play: interaction gone', err);
    else logger.error(`/scrabble-play ${sub} failed`, err);
    await safeReply(interaction, { embeds: [errorEmbed('Something Went Wrong', 'That move could not be completed. Please try again.')], ephemeral: true });
  }
}
