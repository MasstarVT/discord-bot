import { SlashCommandBuilder, ActionRowBuilder, ButtonBuilder, ButtonStyle, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { infoEmbed, warnEmbed } from '../../utils/embedBuilder.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const activeGames = new Set();

const WIN_CONDITIONS = [
  [0, 1, 2], [3, 4, 5], [6, 7, 8], // rows
  [0, 3, 6], [1, 4, 7], [2, 5, 8], // columns
  [0, 4, 8], [2, 4, 6],            // diagonals
];

export const data = new SlashCommandBuilder()
  .setName('tictactoe')
  .setDescription('Challenge another player to Tic-Tac-Toe!')
  .addUserOption((o) =>
    o.setName('target').setDescription('The player you want to challenge').setRequired(true)
  );

// Builds the 3×3 button grid. Filled cells are disabled; all cells disabled when `disabled=true`.
function buildBoard(board, id, disabled = false) {
  const rows = [];
  for (let r = 0; r < 3; r++) {
    const btns = [];
    for (let c = 0; c < 3; c++) {
      const idx  = r * 3 + c;
      const mark = board[idx];
      btns.push(
        new ButtonBuilder()
          .setCustomId(`ttt:${id}:${idx}`)
          .setLabel(mark || '⬜')
          .setStyle(mark === 'X' ? ButtonStyle.Danger : mark === 'O' ? ButtonStyle.Primary : ButtonStyle.Secondary)
          .setDisabled(disabled || Boolean(mark)),
      );
    }
    rows.push(new ActionRowBuilder().addComponents(btns));
  }
  return rows;
}

function checkWinner(board) {
  for (const [a, b, c] of WIN_CONDITIONS) {
    if (board[a] && board[a] === board[b] && board[a] === board[c]) return board[a];
  }
  return board.every(Boolean) ? 'draw' : null;
}

export async function execute(interaction) {
  const challenger = interaction.user;
  const target     = interaction.options.getUser('target');

  if (target.id === challenger.id) {
    return interaction.reply({ content: '❌ You cannot challenge yourself!', ephemeral: true });
  }
  if (target.bot) {
    return interaction.reply({ content: '❌ You cannot challenge a bot!', ephemeral: true });
  }
  if (activeGames.has(challenger.id)) {
    return interaction.reply({ content: '⚠️ You already have an active Tic-Tac-Toe game!', ephemeral: true });
  }
  if (activeGames.has(target.id)) {
    return interaction.reply({ content: `⚠️ ${target.username} is already in a game!`, ephemeral: true });
  }

  activeGames.add(challenger.id);
  activeGames.add(target.id);

  // ── Phase 1: Challenge ────────────────────────────────────────────────────────

  const challengeRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(`ttt-ch:${interaction.id}:accept`).setLabel('✅ Accept').setStyle(ButtonStyle.Success),
    new ButtonBuilder().setCustomId(`ttt-ch:${interaction.id}:decline`).setLabel('❌ Decline').setStyle(ButtonStyle.Danger),
  );

  await interaction.reply({
    content: `<@${target.id}>`,
    embeds: [infoEmbed('Tic-Tac-Toe Challenge', `**${challenger.username}** challenges you to Tic-Tac-Toe! You have **30 seconds** to respond.`)],
    components: [challengeRow],
  });

  const challengeCollector = interaction.channel.createMessageComponentCollector({ time: 30_000 });

  challengeCollector.on('collect', async (i) => {
    if (!i.customId.startsWith(`ttt-ch:${interaction.id}:`)) return;

    if (i.user.id !== target.id) {
      return i.reply({ content: "❌ This challenge isn't for you!", ephemeral: true });
    }

    const action = i.customId.split(':')[2];

    if (action === 'decline') {
      activeGames.delete(challenger.id);
      activeGames.delete(target.id);
      await i.update({
        content: '',
        embeds: [infoEmbed('Challenge Declined', `${target.username} declined the challenge.`)],
        components: [],
      });
      challengeCollector.stop('decline');
      return;
    }

    // ── Phase 2: Game ─────────────────────────────────────────────────────────────

    const board       = Array(9).fill('');
    const marks       = { [challenger.id]: 'X', [target.id]: 'O' };
    let currentPlayer = challenger; // challenger is X and goes first

    const turnEmbed = () =>
      new EmbedBuilder()
        .setColor(COLORS.INFO)
        .setTitle('Tic-Tac-Toe')
        .setDescription(
          `**${challenger.username}** ❌ vs **${target.username}** 🔵\n\n` +
          `It's **${currentPlayer.username}**'s turn (${marks[currentPlayer.id]})`,
        )
        .setTimestamp();

    await i.update({
      content: '',
      embeds: [turnEmbed()],
      components: buildBoard(board, interaction.id),
    });

    const gameCollector = interaction.channel.createMessageComponentCollector({ time: 60_000 });

    gameCollector.on('collect', async (gi) => {
      if (!gi.customId.startsWith(`ttt:${interaction.id}:`)) return;

      if (gi.user.id !== currentPlayer.id) {
        return gi.reply({ content: `⏳ It's **${currentPlayer.username}**'s turn!`, ephemeral: true });
      }

      const idx = parseInt(gi.customId.split(':')[2], 10);
      if (board[idx]) return gi.deferUpdate(); // defensive; button should be disabled

      board[idx]   = marks[currentPlayer.id];
      const result = checkWinner(board);

      if (result === 'draw') {
        await gi.update({
          embeds: [infoEmbed('Tic-Tac-Toe — Draw!', "It's a draw! Well played, both of you.")],
          components: buildBoard(board, interaction.id, true),
        });
        gameCollector.stop('done');
        return;
      }

      if (result) {
        const winEmbed = new EmbedBuilder()
          .setColor(COLORS.SUCCESS)
          .setTitle('🎉 Tic-Tac-Toe — Winner!')
          .setDescription(`**${currentPlayer.username}** wins with **${result}**s!`)
          .setTimestamp();
        await gi.update({ embeds: [winEmbed], components: buildBoard(board, interaction.id, true) });
        gameCollector.stop('done');
        return;
      }

      // Switch turns
      currentPlayer = currentPlayer.id === challenger.id ? target : challenger;
      await gi.update({ embeds: [turnEmbed()], components: buildBoard(board, interaction.id) });
    });

    gameCollector.on('end', async (_, reason) => {
      activeGames.delete(challenger.id);
      activeGames.delete(target.id);
      if (reason === 'done') return;
      await interaction.editReply({
        embeds: [warnEmbed('Tic-Tac-Toe — Timed Out', 'The game timed out due to inactivity.')],
        components: buildBoard(board, interaction.id, true),
      }).catch(() => {});
    });

    challengeCollector.stop('accept');
  });

  challengeCollector.on('end', async (_, reason) => {
    if (reason === 'accept' || reason === 'decline') return;
    activeGames.delete(challenger.id);
    activeGames.delete(target.id);
    await interaction.editReply({
      content: '',
      embeds: [warnEmbed('Challenge Timed Out', `${target.username} did not respond in time.`)],
      components: [],
    }).catch(() => {});
  });
}
