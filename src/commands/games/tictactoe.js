import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, ComponentType, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { buildCustomId, parseCustomId } from '../../config/customIds.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { guildEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { isInteractionGone } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { clampText } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

/** userId → true while the user is in a challenge or a game (the challenger and the target both). */
const activeGames = new Set();

export const WIN_CONDITIONS = [
  [0, 1, 2], [3, 4, 5], [6, 7, 8], // rows
  [0, 3, 6], [1, 4, 7], [2, 5, 8], // columns
  [0, 4, 8], [2, 4, 6],            // diagonals
];

// R022/R091: the game ends after this long WITHOUT a click (reset by every collected click), with a
// hard cap that keeps challenge (30 s) + game inside the 15-minute interaction token.
export const MOVE_IDLE_MS = 60_000;
export const GAME_MAX_MS = 10 * 60_000;
const CHALLENGE_MS = 30_000;

export const data = new SlashCommandBuilder()
  .setName('tictactoe')
  .setDescription('Challenge another player to Tic-Tac-Toe!')
  .addUserOption((o) =>
    o.setName('target').setDescription('The player you want to challenge').setRequired(true)
  );

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

// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
const loadSettings = (interaction) => peekSettings(interaction.guildId)
  ?? withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'tictactoe settings').catch(() => null);

const shownName = (member, user) => escapeMarkdown(clampText(member?.displayName ?? user.username, 32)) || 'Player';

export async function execute(interaction) {
  const challenger = interaction.user;
  const target = interaction.options.getUser('target');
  const targetMember = interaction.options.getMember('target');
  const refuse = (content) => interaction.reply({ content, ephemeral: true });

  if (target.id === challenger.id) return refuse('❌ You cannot challenge yourself!');
  if (target.bot) return refuse('❌ You cannot challenge a bot!');
  if (!targetMember) return refuse("❌ That user isn't in this server.");
  if (activeGames.has(challenger.id)) return refuse('⚠️ You already have an active Tic-Tac-Toe game!');
  const challengerName = shownName(interaction.member, challenger);
  const targetName = shownName(targetMember, target);
  if (activeGames.has(target.id)) return refuse(`⚠️ **${targetName}** is already in a game!`);

  const settings = await loadSettings(interaction);
  // The settings lookup may have yielded: re-check, then take both locks with no await in between.
  if (activeGames.has(challenger.id)) return refuse('⚠️ You already have an active Tic-Tac-Toe game!');
  if (activeGames.has(target.id)) return refuse(`⚠️ **${targetName}** is already in a game!`);
  activeGames.add(challenger.id);
  activeGames.add(target.id);
  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    activeGames.delete(challenger.id);
    activeGames.delete(target.id);
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
      time: CHALLENGE_MS,
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
          idle: MOVE_IDLE_MS,
          time: GAME_MAX_MS,
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
