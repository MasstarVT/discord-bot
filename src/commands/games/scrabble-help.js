import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { dictionaryStatus, loadDictionary } from '../../services/scrabbleService.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { withTimeout } from '../../utils/async.js';
import { guildEmbed } from '../../utils/embedBuilder.js';
import { safeDefer } from '../../utils/interactionHelpers.js';
import { formatNumber } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('scrabble-help')
  .setDescription('Learn how to play Scrabble and use its commands.');

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
    'Tip: use /scrabble-play word with direction:Vertical to build down from an existing tile.',
  ).addFields(
    {
      name: '📋 Commands',
      value: [
        '`/scrabble @user` — challenge someone (30 s to accept)',
        '`/scrabble-play word` — place a word',
        '`/scrabble-play pass` — skip your turn',
        '`/scrabble-play exchange` — swap tiles with the bag (needs 7+ in the bag)',
        '`/scrabble-play resign` — forfeit the game',
        '`/scrabble-help` — this help',
        'Click **🎒 View My Tiles** on the board to see your rack privately.',
      ].join('\n'),
    },
    {
      name: '📍 Placing Words',
      value: [
        'Give the **starting square** and a **direction**:',
        '• **Row** `1`–`15` (top → bottom) · **Column** `A`–`O` (left → right)',
        '• **Direction** — Horizontal (→) or Vertical (↓)',
        'Example: `/scrabble-play word word:HELLO row:8 col:H direction:Horizontal`',
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

export async function execute(interaction) {
  // Acknowledge first (a public deferred reply), then take the time the dictionary status may need.
  if (!(await safeDefer(interaction))) return;
  const settings = peekSettings(interaction.guildId) ?? await getSettingsSafe(interaction.guildId);
  // Starts the lazy load if nobody has yet, and waits briefly so the status line is accurate.
  const status = await withTimeout(loadDictionary(), 2000, 'scrabble dictionary')
    .then(() => dictionaryStatus(), () => dictionaryStatus());
  await interaction.editReply({ embeds: [buildHelpEmbed(settings, status)] });
}
