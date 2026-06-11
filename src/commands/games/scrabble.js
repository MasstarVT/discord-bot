import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { infoEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import {
  activeGames, createBoard, createTileBag, drawTiles,
  buildGameEmbed, RACK_ROW, startTurnTimer, endGame,
} from '../../services/scrabbleService.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('scrabble')
  .setDescription('Challenge another player to a game of Scrabble!')
  .addUserOption((o) =>
    o.setName('target').setDescription('The player you want to challenge').setRequired(true)
  );

export async function execute(interaction, client) {
  const challenger = interaction.user;
  const target     = interaction.options.getUser('target');
  const channelId  = interaction.channelId;

  if (target.id === challenger.id)
    return interaction.reply({ content: '❌ You cannot challenge yourself!', ephemeral: true });
  if (target.bot)
    return interaction.reply({ content: '❌ You cannot challenge a bot!', ephemeral: true });
  if (activeGames.has(channelId))
    return interaction.reply({ content: '⚠️ There is already an active Scrabble game in this channel!', ephemeral: true });

  // Register the persistent "View My Tiles" button handler once per bot lifetime
  if (!client.buttonHandlers.has('scrabble-rack')) {
    client.buttonHandlers.set('scrabble-rack', async (btnInt) => {
      const game = activeGames.get(btnInt.channelId);
      if (!game)
        return btnInt.reply({ content: 'There is no active Scrabble game in this channel.', ephemeral: true });
      const p = game.players[btnInt.user.id];
      if (!p)
        return btnInt.reply({ content: "You're not a player in this game.", ephemeral: true });
      const tiles = p.rack.map((t) => `**${t.letter}** (${t.points})`).join('  ');
      return btnInt.reply({
        content: `🎒 Your tiles: ${tiles || '_(empty)_'}\n📊 Your score: **${p.score}** pts`,
        ephemeral: true,
      });
    });
  }

  // ── Challenge phase ─────────────────────────────────────────────────────────

  const { ActionRowBuilder, ButtonBuilder, ButtonStyle } = await import('discord.js');
  const challengeRow = new ActionRowBuilder().addComponents(
    new ButtonBuilder()
      .setCustomId(`scrabble-ch:${interaction.id}:accept`)
      .setLabel('✅ Accept')
      .setStyle(ButtonStyle.Success),
    new ButtonBuilder()
      .setCustomId(`scrabble-ch:${interaction.id}:decline`)
      .setLabel('❌ Decline')
      .setStyle(ButtonStyle.Danger),
  );

  await interaction.reply({
    content: `<@${target.id}>`,
    embeds: [infoEmbed('Scrabble Challenge', `**${challenger.username}** challenges you to Scrabble! You have **30 seconds** to respond.`)],
    components: [challengeRow],
  });

  const challengeCollector = interaction.channel.createMessageComponentCollector({ time: 30_000 });

  challengeCollector.on('collect', async (i) => {
    if (!i.customId.startsWith(`scrabble-ch:${interaction.id}:`)) return;

    if (i.user.id !== target.id)
      return i.reply({ content: "❌ This challenge isn't for you!", ephemeral: true });

    const action = i.customId.split(':')[2];

    if (action === 'decline') {
      await i.update({
        content: '',
        embeds: [infoEmbed('Challenge Declined', `${target.username} declined the Scrabble challenge.`)],
        components: [],
      });
      challengeCollector.stop('decline');
      return;
    }

    // ── Game start ────────────────────────────────────────────────────────────

    const board = createBoard();
    const bag   = createTileBag();

    const game = {
      board,
      bag,
      players: {
        [challenger.id]: { user: challenger, rack: drawTiles(bag, 7), score: 0 },
        [target.id]:     { user: target,     rack: drawTiles(bag, 7), score: 0 },
      },
      turnOrder:   [challenger.id, target.id],
      currentTurn: 0,
      firstTurn:   true,
      turnTimer:   null,
      gameMessage: null,
      channelId,
    };

    activeGames.set(channelId, game);

    await i.update({
      content: '',
      embeds: [buildGameEmbed(game)],
      components: [RACK_ROW],
    });

    game.gameMessage = await interaction.fetchReply();
    game.turnTimer   = startTurnTimer(channelId, interaction.channel);

    challengeCollector.stop('accept');
  });

  challengeCollector.on('end', async (_, reason) => {
    if (reason === 'accept' || reason === 'decline') return;
    // Timed out
    await interaction.editReply({
      content: '',
      embeds: [warnEmbed('Challenge Timed Out', `${target.username} did not respond in time.`)],
      components: [],
    }).catch(() => {});
  });
}
