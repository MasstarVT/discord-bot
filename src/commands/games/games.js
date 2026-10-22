// /games (pkg-games): browse the games (`list`) and play the ones that need no setup: Rock Paper Scissors (`rps`), the
// number guess (`numguess`) and Tic-Tac-Toe (`tictactoe`). Scrabble has its own /scrabble; the casino games (/blackjack,
// /roulette, /slots) are separate commands and show up in `list`.
import { randomInt } from 'node:crypto';
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType, SlashCommandBuilder, SlashCommandSubcommandBuilder,
  escapeMarkdown,
} from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import { MODULES, isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import {
  applyGuildColor, errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed,
} from '../../utils/embedBuilder.js';
import { isInteractionGone, missingBotPermissions, replyError } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { chunkLines, clampText } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

/** Keys are the command (or `command subcommand`) paths the list shows. */
export const GAME_EMOJI = Object.freeze({
  blackjack: '🃏',
  'games numguess': '🔢',
  roulette: '🎡',
  'games rps': '🪨',
  slots: '🎰',
  'games tictactoe': '⭕',
  scrabble: '🔤',
  'scrabble start': '🔤',
  'scrabble word': '📝',
  'scrabble pass': '⏭️',
  'scrabble exchange': '🔄',
  'scrabble resign': '🏳️',
  'scrabble help': '❓',
});

const isScrabble = (name) => name === 'scrabble' || name.startsWith('scrabble ');

export const MAX_GUESSES = 7;

export const data = new SlashCommandBuilder()
  .setName('games')
  .setDescription('Browse the games, or play Rock Paper Scissors, a number guess or Tic-Tac-Toe.')
  .addSubcommand((s) => s.setName('list').setDescription('Browse all games available to play.'))
  .addSubcommand((s) => s.setName('rps').setDescription('Play Rock, Paper, Scissors against the bot!'))
  .addSubcommand((s) => s.setName('numguess').setDescription(`Guess the bot's number (1–100) in ${MAX_GUESSES} tries!`))
  .addSubcommand((s) => s.setName('tictactoe').setDescription('Challenge another player to Tic-Tac-Toe!')
    .addUserOption((o) =>
      o.setName('target').setDescription('The player you want to challenge').setRequired(true)
    ));

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction, label) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), `${label} settings`).catch(() => null);

// ── /games list ──────────────────────────────────────────────────────────────

const MAX_FIELDS = 25;
const EMBED_BUDGET = 5500; // stay clear of Discord's 6,000-character embed total

/**
 * Every command in the `games` category (or declaring the `games` module). A command with subcommands is listed
 * once per subcommand (`games rps`, `scrabble word`), except /games list itself.
 * Read-only: reads other packages' command metadata from `client.commands`.
 * @param {{ commands: Map<string, object>, commandCategories?: Map<string, string> }} client
 * @returns {Array<{ name: string, description: string, module: string }>}
 */
export function collectGameCommands(client) {
  const entries = [];
  for (const [name, mod] of client.commands) {
    const category = client.commandCategories?.get(name) ?? 'general';
    if (category !== 'games' && mod.module !== 'games') continue;
    const module = mod.module ?? 'games';
    const subs = (mod.data?.options ?? [])
      .filter((o) => o instanceof SlashCommandSubcommandBuilder && !(name === 'games' && o.name === 'list'));
    if (subs.length) {
      for (const s of subs) entries.push({ name: `${name} ${s.name}`, description: s.description ?? '', module });
    } else if (name !== 'games') {
      entries.push({ name, description: mod.data?.description ?? '', module });
    }
  }
  return entries;
}

const byName = (a, b) => a.name.localeCompare(b.name);
const SCRABBLE_ORDER = ['scrabble start', 'scrabble word', 'scrabble pass', 'scrabble exchange', 'scrabble resign', 'scrabble help'];

/**
 * @param {Array<{ name: string, description: string, module: string }>} entries
 * @param {object|null} settings GuildSettings (embed colour only)
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildGamesEmbed(entries, settings) {
  const embed = guildEmbed(settings, '🎮 Games', 'Here are all the games you can play!');
  if (!entries.length) {
    return embed.setDescription('No games are available on this server right now.');
  }

  // Groups, in display order; empty groups are omitted.
  const groups = [];
  groups.push({
    label: '🕹️ Games',
    items: entries.filter((e) => e.module === 'games' && !isScrabble(e.name)).sort(byName),
  });
  groups.push({
    label: '🔤 Scrabble',
    items: entries.filter((e) => isScrabble(e.name))
      .sort((a, b) => SCRABBLE_ORDER.indexOf(a.name) - SCRABBLE_ORDER.indexOf(b.name)),
  });
  const casino = entries.filter((e) => e.module === 'economy').sort(byName);
  groups.push({ label: '🎰 Casino', items: casino });
  const others = [...new Set(entries.map((e) => e.module))]
    .filter((m) => m !== 'games' && m !== 'economy');
  for (const m of others) {
    const info = MODULES[m];
    groups.push({
      label: info ? `${info.emoji} ${info.label}` : `🎮 ${m}`,
      items: entries.filter((e) => e.module === m).sort(byName),
    });
  }

  // Lay the groups out as fields. A very long listing shortens the descriptions (100, then 50, then 20
  // characters) so every group still fits; whatever still does not fit is dropped.
  let layout = [];
  for (const descLimit of [100, 50, 20]) {
    layout = [];
    let size = 0;
    let truncated = false;
    outer:
    for (const group of groups) {
      if (!group.items.length) continue;
      const lines = group.items.map((e) => `${GAME_EMOJI[e.name] ?? '🎮'} \`/${e.name}\` — ${clampText(e.description, descLimit)}`);
      const chunks = chunkLines(lines, 1024);
      for (let i = 0; i < chunks.length; i++) {
        const name = i === 0 ? group.label : `${group.label} (cont.)`;
        if (layout.length >= MAX_FIELDS || size + name.length + chunks[i].length > EMBED_BUDGET) {
          truncated = true;
          break outer;
        }
        layout.push({ name, value: chunks[i] });
        size += name.length + chunks[i].length;
      }
    }
    if (!truncated) break;
  }
  if (layout.length) embed.addFields(layout);

  if (casino.length) embed.setFooter({ text: "Casino games use this server's coins — see /balance." });
  return embed;
}

async function executeList(interaction, client) {
  // Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
  const settings = peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'games settings').catch(() => null);

  // Hides the casino games when Economy is off (and any other module that is off).
  const entries = collectGameCommands(client).filter((e) => settings == null || isModuleEnabled(settings, e.module));
  await interaction.reply({ embeds: [buildGamesEmbed(entries, settings)] });
}

// ── /games rps ───────────────────────────────────────────────────────────────

/** userId → true while the user has a game in progress. */
const rpsActive = new Set();

export const RPS_CHOICES = ['rock', 'paper', 'scissors'];
export const RPS_EMOJI = { rock: '🪨', paper: '📄', scissors: '✂️' };
// RPS_BEATS[x] = what x defeats
export const RPS_BEATS = { rock: 'scissors', paper: 'rock', scissors: 'paper' };

/** @returns {'win'|'lose'|'draw'} the outcome for the player */
export function decideRps(player, bot) {
  if (player === bot) return 'draw';
  return RPS_BEATS[player] === bot ? 'win' : 'lose';
}

export function buildRpsRow(id, disabled = false) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'rock')).setLabel('🪨 Rock').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'paper')).setLabel('📄 Paper').setStyle(ButtonStyle.Primary).setDisabled(disabled),
    new ButtonBuilder().setCustomId(buildCustomId('rps', id, 'scissors')).setLabel('✂️ Scissors').setStyle(ButtonStyle.Primary).setDisabled(disabled),
  );
}

const RPS_TITLE = '✂️ Rock, Paper, Scissors';

async function executeRps(interaction) {
  const userId = interaction.user.id;

  if (rpsActive.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active RPS game!', ephemeral: true });
  }
  const settings = await loadSettings(interaction, 'rps');
  if (rpsActive.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active RPS game!', ephemeral: true });
  }
  rpsActive.add(userId);

  // R089: the collector's `end` releases the lock; release it here if anything throws before it exists.
  let handedOff = false;
  try {
    const response = await interaction.reply({
      embeds: [guildEmbed(settings, RPS_TITLE, 'Choose your move! You have **30 seconds**.')],
      components: [buildRpsRow(interaction.id)],
    });

    // The filter checks only this game's prefix; the user is checked inside `collect`.
    const collector = response.createMessageComponentCollector({
      componentType: ComponentType.Button,
      filter: (i) => i.customId.startsWith(`rps:${interaction.id}:`),
      time: 30_000,
    });
    let played = false;

    collector.on('collect', async (i) => {
      try {
        if (i.user.id !== userId) {
          await i.reply({ content: "❌ This isn't your game!", ephemeral: true });
          return;
        }
        // R237: a second press before the first update lands is acknowledged and ignored.
        if (played) return void i.deferUpdate().catch(() => {});
        const choice = parseCustomId(i.customId).params[1];
        if (!RPS_CHOICES.includes(choice)) return void i.deferUpdate().catch(() => {});

        played = true;
        collector.stop('done'); // synchronously, before any await

        const bot = RPS_CHOICES[randomInt(RPS_CHOICES.length)];
        const description = `You: ${RPS_EMOJI[choice]} **${choice}**\nBot: ${RPS_EMOJI[bot]} **${bot}**`;
        const outcome = decideRps(choice, bot);
        const embed = outcome === 'win' ? successEmbed('You win!', description)
          : outcome === 'lose' ? errorEmbed('You lose!', description)
            : guildEmbed(settings, "🤝 It's a draw!", description);
        await i.update({ embeds: [embed], components: [buildRpsRow(interaction.id, true)] });
      } catch (err) {
        if (isInteractionGone(err)) logger.debug('rps: interaction gone', err);
        else logger.error('RPS collect failed', err);
        if (i && !i.replied && !i.deferred) await i.deferUpdate().catch(() => {});
      }
    });

    collector.on('end', async (_collected, reason) => {
      try {
        rpsActive.delete(userId);
        if (reason !== 'time') return;
        await interaction.editReply({
          embeds: [guildEmbed(settings, RPS_TITLE, "⏰ Time's up! No move was made.")],
          components: [buildRpsRow(interaction.id, true)],
        }).catch(() => {});
      } catch (err) {
        logger.error('RPS end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) rpsActive.delete(userId);
  }
}

// ── /games numguess ──────────────────────────────────────────────────────────

/** userId → true while the user has a game in progress. */
const numguessActive = new Set();

export const GUESS_IDLE_MS = 60_000;         // disclosed in the intro; reset by every guess (R234)
export const NUMGUESS_MAX_MS = 10 * 60_000;  // hard cap, below the 15-minute interaction token lifetime
const NUMGUESS_WARN_EVERY_MS = 5_000;

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

async function executeNumguess(interaction) {
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
  if (numguessActive.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active guessing game!', ephemeral: true });
  }

  const secret = randomInt(1, 101);
  const settings = await loadSettings(interaction, 'numguess');
  if (numguessActive.has(userId)) {
    return interaction.reply({ content: '⚠️ You already have an active guessing game!', ephemeral: true });
  }
  numguessActive.add(userId);

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
      time: NUMGUESS_MAX_MS,
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
          if (now - lastWarnAt < NUMGUESS_WARN_EVERY_MS) return;
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
        numguessActive.delete(userId);
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
      numguessActive.delete(userId);
      collector?.stop('error');
    }
  }
}

// ── /games tictactoe ─────────────────────────────────────────────────────────

/** userId → true while the user is in a challenge or a game (the challenger and the target both). */
const tttActive = new Set();

export const WIN_CONDITIONS = [
  [0, 1, 2], [3, 4, 5], [6, 7, 8], // rows
  [0, 3, 6], [1, 4, 7], [2, 5, 8], // columns
  [0, 4, 8], [2, 4, 6],            // diagonals
];

// R022/R091: the game ends after this long WITHOUT a click (reset by every collected click), with a
// hard cap that keeps challenge (30 s) + game inside the 15-minute interaction token.
export const TTT_MOVE_IDLE_MS = 60_000;
export const TTT_MAX_MS = 10 * 60_000;
const TTT_CHALLENGE_MS = 30_000;

/** Builds the 3×3 button grid. Filled cells are disabled; every cell is disabled when `disabled`. */
export function buildBoard(board, id, disabled = false) {
  const rows = [];
  for (let r = 0; r < 3; r++) {
    const btns = [];
    for (let c = 0; c < 3; c++) {
      const idx = r * 3 + c;
      const mark = board[idx];
      btns.push(
        new ButtonBuilder()
          .setCustomId(buildCustomId('ttt', id, String(idx)))
          .setLabel(mark || '⬜')
          .setStyle(mark === 'X' ? ButtonStyle.Danger : mark === 'O' ? ButtonStyle.Primary : ButtonStyle.Secondary)
          .setDisabled(disabled || Boolean(mark)),
      );
    }
    rows.push(new ActionRowBuilder().addComponents(btns));
  }
  return rows;
}

/** @returns {'X'|'O'|'draw'|null} */
export function checkWinner(board) {
  for (const [a, b, c] of WIN_CONDITIONS) {
    if (board[a] && board[a] === board[b] && board[a] === board[c]) return board[a];
  }
  return board.every(Boolean) ? 'draw' : null;
}

const shownName = (member, user) => escapeMarkdown(clampText(member?.displayName ?? user.username, 32)) || 'Player';

async function executeTictactoe(interaction) {
  const challenger = interaction.user;
  const target = interaction.options.getUser('target');
  const targetMember = interaction.options.getMember('target');
  const refuse = (content) => interaction.reply({ content, ephemeral: true });

  if (target.id === challenger.id) return refuse('❌ You cannot challenge yourself!');
  if (target.bot) return refuse('❌ You cannot challenge a bot!');
  if (!targetMember) return refuse("❌ That user isn't in this server.");
  if (tttActive.has(challenger.id)) return refuse('⚠️ You already have an active Tic-Tac-Toe game!');
  const challengerName = shownName(interaction.member, challenger);
  const targetName = shownName(targetMember, target);
  if (tttActive.has(target.id)) return refuse(`⚠️ **${targetName}** is already in a game!`);

  const settings = await loadSettings(interaction, 'tictactoe');
  // The settings lookup may have yielded: re-check, then take both locks with no await in between.
  if (tttActive.has(challenger.id)) return refuse('⚠️ You already have an active Tic-Tac-Toe game!');
  if (tttActive.has(target.id)) return refuse(`⚠️ **${targetName}** is already in a game!`);
  tttActive.add(challenger.id);
  tttActive.add(target.id);
  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    tttActive.delete(challenger.id);
    tttActive.delete(target.id);
  };

  // R089: the locks belong to the collectors once they exist; release them if anything throws before.
  let handedOff = false;
  try {
    // ── Phase 1: challenge ────────────────────────────────────────────────────
    const challengeRow = new ActionRowBuilder().addComponents(
      new ButtonBuilder().setCustomId(buildCustomId('ttt-ch', interaction.id, 'accept')).setLabel('✅ Accept').setStyle(ButtonStyle.Success),
      new ButtonBuilder().setCustomId(buildCustomId('ttt-ch', interaction.id, 'decline')).setLabel('❌ Decline').setStyle(ButtonStyle.Danger),
    );
    const response = await interaction.reply({
      content: `<@${target.id}>`,
      allowedMentions: { users: [target.id] },
      embeds: [guildEmbed(settings, '⭕ Tic-Tac-Toe Challenge', `**${challengerName}** challenges you to Tic-Tac-Toe! You have **30 seconds** to respond.`)],
      components: [challengeRow],
    });

    // Filters check only this game's prefix; users are checked inside `collect`, so a wrong user's click
    // gets a friendly reply instead of the router's "expired" fallback.
    const challengeCollector = response.createMessageComponentCollector({
      componentType: ComponentType.Button,
      filter: (i) => i.customId.startsWith(`ttt-ch:${interaction.id}:`),
      time: TTT_CHALLENGE_MS,
    });
    let settled = false;
    let gameStarted = false;

    challengeCollector.on('collect', async (i) => {
      try {
        const action = parseCustomId(i.customId).params[1];

        if (settled) return void i.deferUpdate().catch(() => {});

        if (i.user.id === challenger.id) {
          if (action === 'decline') {
            // The challenger "declining" cancels their own challenge.
            settled = true;
            challengeCollector.stop('cancel');
            release();
            await i.update({
              content: '',
              embeds: [guildEmbed(settings, '⭕ Tic-Tac-Toe', `**${challengerName}** cancelled the challenge.`)],
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
          challengeCollector.stop('decline');
          release();
          await i.update({
            content: '',
            embeds: [guildEmbed(settings, '⭕ Tic-Tac-Toe', `${targetName} declined the challenge.`)],
            components: [],
          });
          return;
        }

        if (action !== 'accept') return void i.deferUpdate().catch(() => {});

        // ── Accept (R090): decide before any await, so a double tap starts one game ──
        settled = true;
        challengeCollector.stop('accept');

        const board = Array(9).fill('');
        const marks = { [challenger.id]: 'X', [target.id]: 'O' };
        const names = { [challenger.id]: challengerName, [target.id]: targetName };
        let current = challenger.id; // the challenger is X and goes first
        let finished = false;

        const turnEmbed = () => guildEmbed(
          settings,
          '⭕ Tic-Tac-Toe',
          `**${challengerName}** ❌ vs **${targetName}** 🔵\n\nIt's **${names[current]}**'s turn (${marks[current]}) · 60 s per move`,
        );

        // The game collector exists before the update lands, so no click is ever left unhandled.
        const gameCollector = response.createMessageComponentCollector({
          componentType: ComponentType.Button,
          filter: (gi) => gi.customId.startsWith(`ttt:${interaction.id}:`),
          idle: TTT_MOVE_IDLE_MS,
          time: TTT_MAX_MS,
        });

        gameStarted = true;

        gameCollector.on('collect', async (gi) => {
          try {
            if (finished) return void gi.deferUpdate().catch(() => {}); // R242

            if (!(gi.user.id in marks)) { // R241
              await gi.reply({ content: "❌ You're not a player in this game!", ephemeral: true });
              return;
            }
            if (gi.user.id !== current) {
              await gi.reply({ content: `⏳ It's **${names[current]}**'s turn!`, ephemeral: true });
              return;
            }

            const idx = Number(parseCustomId(gi.customId).params[1]);
            if (!Number.isInteger(idx) || idx < 0 || idx > 8 || board[idx]) {
              return void gi.deferUpdate().catch(() => {});
            }

            board[idx] = marks[current];
            const result = checkWinner(board);

            if (result === 'draw') {
              finished = true;
              gameCollector.stop('done');
              await gi.update({
                embeds: [guildEmbed(settings, '🤝 Tic-Tac-Toe — Draw!', "It's a draw! Well played, both of you.")],
                components: buildBoard(board, interaction.id, true),
              });
              return;
            }
            if (result) {
              finished = true;
              gameCollector.stop('done');
              await gi.update({
                embeds: [successEmbed('Tic-Tac-Toe — Winner!', `🎉 **${names[current]}** wins with **${result}**s!`)],
                components: buildBoard(board, interaction.id, true),
              });
              return;
            }

            current = current === challenger.id ? target.id : challenger.id;
            await gi.update({ embeds: [turnEmbed()], components: buildBoard(board, interaction.id) });
          } catch (err) {
            if (isInteractionGone(err)) logger.debug('tictactoe: interaction gone', err);
            else logger.error('Tic-Tac-Toe move failed', err);
            if (gi && !gi.replied && !gi.deferred) await gi.deferUpdate().catch(() => {});
          }
        });

        gameCollector.on('end', async (_collected, reason) => {
          try {
            release();
            if (reason === 'done' || reason === 'error') return;
            if (reason !== 'idle' && reason !== 'time') return;
            const text = reason === 'idle'
              ? `**${names[current]}** didn't move within 60 seconds. Game over.`
              : 'The game reached its 10-minute limit.';
            await interaction.editReply({
              content: '',
              embeds: [warnEmbed('Tic-Tac-Toe — Timed Out', text)],
              components: buildBoard(board, interaction.id, true),
            }).catch(() => {});
          } catch (err) {
            logger.error('Tic-Tac-Toe end failed', err);
          }
        });

        try {
          await i.update({ content: '', embeds: [turnEmbed()], components: buildBoard(board, interaction.id) });
        } catch (err) {
          if (isInteractionGone(err)) logger.debug('tictactoe: accept interaction gone', err);
          else logger.error('Tic-Tac-Toe start failed', err);
          gameCollector.stop('error'); // releases the locks through its end handler
        }
      } catch (err) {
        if (isInteractionGone(err)) logger.debug('tictactoe: interaction gone', err);
        else logger.error('Tic-Tac-Toe challenge failed', err);
        if (settled && !gameStarted) release(); // decided, but the game never got its collector
        if (i && !i.replied && !i.deferred) await i.deferUpdate().catch(() => {});
      }
    });

    challengeCollector.on('end', async (_collected, reason) => {
      try {
        if (reason === 'accept') return; // the game collector owns the locks now
        release();
        if (reason === 'time') {
          await interaction.editReply({
            content: '',
            embeds: [warnEmbed('Challenge Timed Out', `${targetName} did not respond in time.`)],
            components: [],
          }).catch(() => {});
        }
      } catch (err) {
        logger.error('Tic-Tac-Toe challenge end failed', err);
      }
    });

    handedOff = true;
  } finally {
    if (!handedOff) release();
  }
}

// ── /games <subcommand> ──────────────────────────────────────────────────────

export async function execute(interaction, client) {
  switch (interaction.options.getSubcommand()) {
    case 'rps': return executeRps(interaction);
    case 'numguess': return executeNumguess(interaction);
    case 'tictactoe': return executeTictactoe(interaction);
    default: return executeList(interaction, client);
  }
}
