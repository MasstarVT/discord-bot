// /scrabble (pkg-games): `start` challenges a player, `word|pass|exchange|resign` are the moves and `help` explains the
// game. The game itself (board, bag, scoring, timers) lives in services/scrabbleService.js.
import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import {
  CHALLENGE_TIMEOUT_MS, COLS, createGame, currentPlayerId, describeWords, dictionaryStatus, exchangeTiles, finishGame,
  formatRack, gameMutex, getDictionary, getGame, isChannelBusy, loadDictionary, passTurn, playWord, postBoard,
  postResult, registerGame, releaseChannel, reserveChannel, resignGame,
} from '../../services/scrabbleService.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm, isInteractionGone, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const COL_CHOICES = 'ABCDEFGHIJKLMNO'.split('').map((c) => ({ name: c, value: c }));

export const data = new SlashCommandBuilder()
  .setName('scrabble')
  .setDescription('Play Scrabble: challenge a player, place words, pass, exchange tiles or resign.')
  .addSubcommand((s) => s.setName('start').setDescription('Challenge another player to a game of Scrabble!')
    .addUserOption((o) =>
      o.setName('target').setDescription('The player you want to challenge').setRequired(true)
    ))
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
  .addSubcommand((s) => s.setName('resign').setDescription('Forfeit the game'))
  .addSubcommand((s) => s.setName('help').setDescription('Learn how to play Scrabble and use its commands.'));

// ── /scrabble start ──────────────────────────────────────────────────────────

// How long an accepted challenge may wait for the dictionary to finish loading (it started at challenge time).
const DICTIONARY_WAIT_MS = 5000;
const BUSY_TEXT = '⚠️ There is already a Scrabble game or a pending challenge in this channel!';

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'scrabble settings').catch(() => null);

const shownName = (member, user) => escapeMarkdown(clampText(member?.displayName ?? user.username, 32)) || 'Player';

async function executeStart(interaction) {
  const challenger = interaction.user;
  const target = interaction.options.getUser('target');
  const channelId = interaction.channelId;
  const refuse = (content) => interaction.reply({ content, ephemeral: true });

  if (target.id === challenger.id) return refuse('❌ You cannot challenge yourself!');
  if (target.bot) return refuse('❌ You cannot challenge a bot!');
  const targetMember = interaction.options.getMember('target');
  if (!targetMember) return refuse("❌ That user isn't in this server.");
  if (isChannelBusy(channelId)) return refuse(BUSY_TEXT);

  void loadDictionary(); // starts the lazy load; memoised, never rejects, deliberately not awaited
  const settings = await loadSettings(interaction);
  const challengerName = shownName(interaction.member, challenger);
  const targetName = shownName(targetMember, target);

  // R085: the reservation is taken synchronously with the busy check, so two challenges can't coexist.
  if (!reserveChannel(channelId, interaction.id)) return refuse(BUSY_TEXT);

  // R089: the reservation belongs to the collector once it exists; release it if anything throws before.
  let handedOff = false;
  try {
    const row = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId(buildCustomId('scrabble-ch', interaction.id, 'accept')).setLabel('✅ Accept').setStyle(ButtonStyle.Success),
      new ButtonBuilder().setCustomId(buildCustomId('scrabble-ch', interaction.id, 'decline')).setLabel('❌ Decline').setStyle(ButtonStyle.Danger),
    );
    const response = await interaction.reply({
      content: `<@${target.id}>`,
      allowedMentions: { users: [target.id] },
      embeds: [guildEmbed(settings, '🔤 Scrabble Challenge', `**${challengerName}** challenges you to Scrabble! You have **30 seconds** to respond.`)],
      components: [row],
    });

    // The filter checks only this challenge's prefix; users are checked inside `collect`, so a wrong
    // user's click gets a friendly reply instead of the router's "expired" fallback.
    const collector = response.createMessageComponentCollector({
      componentType: ComponentType.Button,
      filter: (i) => i.customId.startsWith(`scrabble-ch:${interaction.id}:`),
      time: CHALLENGE_TIMEOUT_MS,
    });
    let settled = false;

    collector.on('collect', async (i) => {
      try {
        const action = parseCustomId(i.customId).params[1];

        if (settled) return void i.deferUpdate().catch(() => {});

        if (i.user.id === challenger.id) {
          if (action === 'decline') {
            // The challenger "declining" cancels their own challenge.
            settled = true;
            collector.stop('cancel');
            releaseChannel(channelId, interaction.id);
            await i.update({
              content: '',
              embeds: [guildEmbed(settings, '🔤 Scrabble', `**${challengerName}** cancelled the challenge.`)],
              components: [],
            });
            return;
          }
          await i.reply({ content: `⏳ Waiting for **${targetName}** to accept.`, ephemeral: true });
          return;
        }

        if (i.user.id !== target.id) {
          await i.reply({ content: "❌ This challenge isn't for you!", ephemeral: true });
          return;
        }

        if (action === 'decline') {
          settled = true;
          collector.stop('decline');
          releaseChannel(channelId, interaction.id);
          await i.update({
            content: '',
            embeds: [guildEmbed(settings, '🔤 Scrabble', `${targetName} declined the Scrabble challenge.`)],
            components: [],
          });
          return;
        }

        if (action !== 'accept') return void i.deferUpdate().catch(() => {});

        // Accept (R021, R086, R087): decide synchronously, so a second Accept can't start a second game.
        settled = true;
        collector.stop('accept');

        // Acknowledge first (00-overview rule 21). The channel stays reserved meanwhile, and `settled`
        // (plus the ended collector) blocks repeat clicks, so nothing can start a second game.
        await i.deferUpdate();

        // The dictionary load began when the challenge was issued, so this is normally instant.
        const dictionary = dictionaryStatus().state === 'loading'
          ? await withTimeout(loadDictionary(), DICTIONARY_WAIT_MS, 'scrabble dictionary').catch(() => null)
          : getDictionary();

        const game = createGame({
          id: interaction.id,
          guildId: interaction.guildId,
          channelId,
          settings,
          dictionary,
          players: [{ id: challenger.id, name: challengerName }, { id: target.id, name: targetName }],
        });
        if (!registerGame(game)) {
          releaseChannel(channelId, interaction.id);
          await i.followUp({ content: '⚠️ A Scrabble game already started in this channel.', ephemeral: true });
          return;
        }
        game.lastInteraction = i;

        let started = false;
        try {
          await i.editReply({
            content: '',
            embeds: [guildEmbed(settings, '🔤 Scrabble', `**${targetName}** accepted **${challengerName}**'s challenge. Good luck!`)],
            components: [],
          });
          started = await postBoard(game, i, {
            content: `<@${challenger.id}>, you go first! Play with \`/scrabble word\` — you have 3 minutes per turn.`,
            mentionIds: [challenger.id],
          });
        } catch (err) {
          if (isInteractionGone(err)) logger.debug('scrabble: accept interaction gone', err);
          else logger.error('Scrabble start failed', err);
        }
        if (!started) {
          // A game that never got its board would lock the channel until its timer ran out.
          finishGame(game, 'aborted');
          i.followUp({ content: "⚠️ I couldn't start the game. Please try `/scrabble start` again.", ephemeral: true }).catch(() => {});
        }
      } catch (err) {
        if (isInteractionGone(err)) logger.debug('scrabble: interaction gone', err);
        else logger.error('Scrabble challenge collect failed', err);
        releaseChannel(channelId, interaction.id);
        if (i && !i.replied && !i.deferred) await i.deferUpdate().catch(() => {});
      }
    });

    collector.on('end', async (_collected, reason) => {
      try {
        if (reason === 'accept' || reason === 'decline' || reason === 'cancel') return;
        releaseChannel(channelId, interaction.id);
        if (reason === 'time') {
          await interaction.editReply({
            content: '',
            embeds: [warnEmbed('Challenge Timed Out', `${targetName} did not respond in time.`)],
            components: [],
          }).catch(() => {});
        }
      } catch (err) {
        logger.error('Scrabble challenge end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) releaseChannel(channelId, interaction.id);
  }
}

// ── /scrabble word, pass, exchange, resign ───────────────────────────────────

const NO_GAME = () => errorEmbed('No Active Game', 'There is no Scrabble game running in this channel. Start one with `/scrabble start`.');
const NOT_PLAYER = () => errorEmbed('Not Your Game', "You're not a player in this game.");

/** Ephemeral edit of the deferred reply; the move is already committed, so a failure is only logged. */
async function tell(interaction, payload) {
  try {
    await interaction.editReply(payload);
  } catch (err) {
    if (isInteractionGone(err)) logger.debug('scrabble: interaction gone', err);
    else logger.warn('scrabble: reply failed', err);
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

async function executePlay(interaction, sub) {
  // R083: acknowledge first, then serialise every move of the channel.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  try {
    if (sub === 'resign') {
      await handleResign(interaction);
      return;
    }
    await gameMutex.run(interaction.channelId, () => handleMove(interaction, sub));
  } catch (err) {
    if (isInteractionGone(err)) logger.debug('scrabble: interaction gone', err);
    else logger.error(`/scrabble ${sub} failed`, err);
    await safeReply(interaction, { embeds: [errorEmbed('Something Went Wrong', 'That move could not be completed. Please try again.')], ephemeral: true });
  }
}

// ── /scrabble help ───────────────────────────────────────────────────────────

/** The "Rules" bullet that tells players whether words are checked. */
export function dictionaryLine(status) {
  return status.state === 'loaded'
    ? `Words are checked against a **${formatNumber(status.size)}-word** list.`
    : 'Word checking is **off**: any letters are accepted.';
}

/**
 * Builds the help embed. Every field value stays within Discord's 1,024-character limit.
 * @param {object|null} settings GuildSettings (embed colour only)
 * @param {{ state: string, size: number }} status dictionaryStatus()
 */
export function buildHelpEmbed(settings, status) {
  return guildEmbed(
    settings,
    '🔤 How to Play Scrabble',
    'A 2-player word game on a 15×15 board. Take turns placing words; the highest score at the end wins.',
    'Tip: use /scrabble word with direction:Vertical to build down from an existing tile.',
  ).addFields(
    {
      name: '📋 Commands',
      value: [
        '`/scrabble start` — challenge someone (30 s to accept)',
        '`/scrabble word` — place a word',
        '`/scrabble pass` — skip your turn',
        '`/scrabble exchange` — swap tiles with the bag (needs 7+ in the bag)',
        '`/scrabble resign` — forfeit the game',
        '`/scrabble help` — this help',
        'Click **🎒 View My Tiles** on the board to see your rack privately.',
      ].join('\n'),
    },
    {
      name: '📍 Placing Words',
      value: [
        'Give the **starting square** and a **direction**:',
        '• **Row** `1`–`15` (top → bottom) · **Column** `A`–`O` (left → right)',
        '• **Direction** — Horizontal (→) or Vertical (↓)',
        'Example: `/scrabble word word:HELLO row:8 col:H direction:Horizontal`',
        'Tiles touching your word are joined to it automatically.',
        '**Blanks:** put `?` before a letter to play a blank as that letter, e.g. `QU?IZ`. Blanks score 0 and show in lowercase.',
      ].join('\n'),
    },
    {
      name: '📏 Rules',
      value: [
        '• The first word must cover the centre square **H8** (★) and be at least 2 letters.',
        '• Later words must touch or pass through tiles already on the board.',
        '• Every word your tiles form counts — the main word and any cross-words.',
        '• Using all 7 tiles in one move earns a **+50 bingo**.',
        '• You have **3 minutes** per turn; running out of time forfeits the game.',
        `• ${dictionaryLine(status)}`,
      ].join('\n'),
    },
    {
      name: '🗺️ Board Squares',
      value: [
        '`TW` Triple Word score',
        '`DW` Double Word score',
        '`TL` Triple Letter score',
        '`DL` Double Letter score',
        '`★ ` Centre square (counts as Double Word on first play)',
        '`· ` Empty square',
        '`a` lowercase letter = blank tile',
      ].join('\n'),
    },
    {
      name: '🔢 Scoring',
      value: [
        'Each letter has a point value (e.g. A=1, Q=10, Z=10).',
        'Letter multipliers (`DL`/`TL`) apply first, then word multipliers (`DW`/`TW`) multiply the total.',
        'Multipliers only apply when a **new tile** is placed on that square — they are consumed after use.',
        'Cross-words formed by your new tiles are scored too.',
      ].join('\n'),
    },
    {
      name: '🏁 Ending the Game',
      value: [
        "• **Out:** the bag is empty and a player uses their last tile — they gain the value of the opponent's tiles, which the opponent loses.",
        '• **Six scoreless turns** in a row (passes/exchanges): each player loses the value of their own tiles.',
        '• **Resign or time-out:** the other player wins.',
      ].join('\n'),
    },
  );
}

async function executeHelp(interaction) {
  // Acknowledge first (a public deferred reply), then take the time the dictionary status may need.
  if (!(await safeDefer(interaction))) return;
  const settings = peekSettings(interaction.guildId) ?? await getSettingsSafe(interaction.guildId);
  // Starts the lazy load if nobody has yet, and waits briefly so the status line is accurate.
  const status = await withTimeout(loadDictionary(), 2000, 'scrabble dictionary')
    .then(() => dictionaryStatus(), () => dictionaryStatus());
  await interaction.editReply({ embeds: [buildHelpEmbed(settings, status)] });
}

// ── /scrabble <subcommand> ───────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'start') return executeStart(interaction);
  if (sub === 'help') return executeHelp(interaction);
  return executePlay(interaction, sub);
}
