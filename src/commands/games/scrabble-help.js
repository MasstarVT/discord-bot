import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('scrabble-help')
  .setDescription('Learn how to play Scrabble and use its commands.');

export async function execute(interaction) {
  const embed = new EmbedBuilder()
    .setColor(COLORS.INFO)
    .setTitle('🔤 How to Play Scrabble')
    .setDescription('A 2-player word game on a 15×15 board. Take turns placing words to score points. Highest score when the bag runs out wins!')
    .addFields(
      {
        name: '📋 Commands',
        value: [
          '`/scrabble @user` — Challenge someone to a game',
          '`/scrabble-play word row col direction` — Play a word on the board',
          '`/scrabble-help` — Show this help message',
          '',
          'During a game, click **🎒 View My Tiles** to see your rack privately.',
        ].join('\n'),
      },
      {
        name: '📍 Placing Words',
        value: [
          'Words are placed by specifying a **starting cell** and a **direction**.',
          '',
          '• **Row** — a number from `1` to `15` (top → bottom)',
          '• **Col** — a letter from `A` to `O` (left → right)',
          '• **Direction** — `Horizontal` (left→right) or `Vertical` (top→down)',
          '',
          '**Example:** `/scrabble-play word:HELLO row:8 col:H direction:Horizontal`',
          'Places H-E-L-L-O starting at H8, going right.',
        ].join('\n'),
      },
      {
        name: '📏 Rules',
        value: [
          '• The **first word** must cover the centre square (**H8** — marked ★) and be at least 2 letters.',
          '• Every word after that must **connect to** or **pass through** a tile already on the board.',
          '• You may reuse existing tiles — only the **new letters** are taken from your rack.',
          '• You have **7 tiles** on your rack at all times (refilled from the bag after each play).',
          '• Using all 7 rack tiles in one move earns a **+50 bingo bonus**.',
          '• You have **3 minutes** per turn or the game ends automatically.',
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
        ].join('\n'),
      },
      {
        name: '🔢 Scoring',
        value: [
          'Each letter has a point value (e.g. A=1, Q=10, Z=10).',
          'Letter multipliers (`DL`/`TL`) apply first, then word multipliers (`DW`/`TW`) multiply the total.',
          'Multipliers only apply when a **new tile** is placed on that square — they are consumed after use.',
        ].join('\n'),
      },
      {
        name: '🏁 Ending the Game',
        value: 'The game ends when the bag is empty and a player uses their last tile, or when a player times out. Remaining rack tiles are deducted from that player\'s score and added to their opponent\'s.',
      },
    )
    .setFooter({ text: 'Tip: use /scrabble-play with direction:Vertical to build down from an existing tile.' })
    .setTimestamp();

  await interaction.reply({ embeds: [embed] });
}
