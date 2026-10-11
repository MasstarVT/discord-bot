import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { MODULES, isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { guildEmbed } from '../../utils/embedBuilder.js';
import { chunkLines, clampText } from '../../utils/text.js';

export const module = 'games';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const GAME_EMOJI = Object.freeze({
  blackjack: '🃏',
  numguess: '🔢',
  roulette: '🎡',
  rps: '🪨',
  slots: '🎰',
  tictactoe: '⭕',
  scrabble: '🔤',
  'scrabble-play': '📝',
  'scrabble-help': '❓',
});

export const SCRABBLE_COMMANDS = new Set(['scrabble', 'scrabble-play', 'scrabble-help']);

const MAX_FIELDS = 25;
const EMBED_BUDGET = 5500; // stay clear of Discord's 6,000-character embed total

export const data = new SlashCommandBuilder()
  .setName('games')
  .setDescription('Browse all games available to play.');

/**
 * Every command in the `games` category (or declaring the `games` module), except /games itself.
 * Read-only: reads other packages' command metadata from `client.commands`.
 * @param {{ commands: Map<string, object>, commandCategories?: Map<string, string> }} client
 * @returns {Array<{ name: string, description: string, module: string }>}
 */
export function collectGameCommands(client) {
  const entries = [];
  for (const [name, mod] of client.commands) {
    if (name === 'games') continue;
    const category = client.commandCategories?.get(name) ?? 'general';
    if (category !== 'games' && mod.module !== 'games') continue;
    entries.push({ name, description: mod.data?.description ?? '', module: mod.module ?? 'games' });
  }
  return entries;
}

const byName = (a, b) => a.name.localeCompare(b.name);
const SCRABBLE_ORDER = ['scrabble', 'scrabble-play', 'scrabble-help'];

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
    items: entries.filter((e) => e.module === 'games' && !SCRABBLE_COMMANDS.has(e.name)).sort(byName),
  });
  groups.push({
    label: '🔤 Scrabble',
    items: entries.filter((e) => SCRABBLE_COMMANDS.has(e.name))
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

export async function execute(interaction, client) {
  // Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
  const settings = peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'games settings').catch(() => null);

  // Hides the casino games when Economy is off (and any other module that is off).
  const entries = collectGameCommands(client).filter((e) => settings == null || isModuleEnabled(settings, e.module));
  await interaction.reply({ embeds: [buildGamesEmbed(entries, settings)] });
}
