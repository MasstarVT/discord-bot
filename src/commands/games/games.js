import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const GAME_EMOJI = {
  blackjack:       '🃏',
  numguess:        '🔢',
  roulette:        '🎡',
  rps:             '✂️',
  slots:           '🎰',
  tictactoe:       '❌',
};

const SCRABBLE_COMMANDS = new Set(['scrabble', 'scrabble-play', 'scrabble-help']);

export const data = new SlashCommandBuilder()
  .setName('games')
  .setDescription('Browse all games available to play.');

export async function execute(interaction, client) {
  const games   = [];
  const scrabble = [];

  for (const [name, mod] of client.commands) {
    if ((client.commandCategories.get(name) ?? 'general') !== 'games') continue;
    if (name === 'games') continue;

    const entry = { name, description: mod.data.description };
    if (SCRABBLE_COMMANDS.has(name)) {
      scrabble.push(entry);
    } else {
      games.push(entry);
    }
  }

  games.sort((a, b) => a.name.localeCompare(b.name));
  scrabble.sort((a, b) => a.name.localeCompare(b.name));

  const embed = new EmbedBuilder()
    .setColor(COLORS.PRIMARY)
    .setTitle('🎮 Games')
    .setDescription('Here are all the games you can play!')
    .setTimestamp();

  if (games.length > 0) {
    const lines = games.map(({ name, description }) => {
      const emoji = GAME_EMOJI[name] ?? '🎮';
      return `${emoji} \`/${name}\` — ${description}`;
    });
    embed.addFields({ name: '🕹️ Available Games', value: lines.join('\n') });
  }

  if (scrabble.length > 0) {
    const lines = scrabble.map(({ name, description }) => `\`/${name}\` — ${description}`);
    embed.addFields({ name: '🔤 Scrabble', value: lines.join('\n') });
  }

  await interaction.reply({ embeds: [embed] });
}
