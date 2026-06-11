import { SlashCommandBuilder, EmbedBuilder } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import prisma from '../../database/client.js';
import { cache } from '../../services/redis.js';

export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('help')
  .setDescription('Show all commands available to you.');

const CATEGORY_LABELS = {
  utility:    { label: '🔧 Utility',    order: 0 },
  leveling:   { label: '⭐ Leveling',   order: 1 },
  moderation: { label: '🔨 Moderation', order: 2 },
  admin:      { label: '⚙️ Admin',      order: 3 },
  general:    { label: '📦 General',    order: 4 },
};

const LEVEL_BADGE = {
  [PERMISSION_LEVELS.MEMBER]:    '',
  [PERMISSION_LEVELS.MODERATOR]: ' *(Mod)*',
  [PERMISSION_LEVELS.ADMIN]:     ' *(Admin)*',
  [PERMISSION_LEVELS.OWNER]:     ' *(Owner)*',
};

export async function execute(interaction, client) {
  await interaction.deferReply({ ephemeral: true });

  // Resolve the caller's permission level
  const cacheKey = `guild:${interaction.guildId}:settings`;
  let settings = await cache.get(cacheKey);
  if (!settings) {
    settings = await prisma.guildSettings.findUnique({ where: { guildId: interaction.guildId } });
    if (settings) await cache.set(cacheKey, settings, 300);
  }

  const userLevel = resolvePermissionLevel(interaction.member, settings);

  // Collect commands the user can access, grouped by category
  const groups = new Map(); // category → [{ name, description, level }]

  for (const [name, mod] of client.commands) {
    const required = mod.permissionLevel ?? PERMISSION_LEVELS.MEMBER;
    if (userLevel < required) continue;

    const category = client.commandCategories.get(name) ?? 'general';
    if (!groups.has(category)) groups.set(category, []);
    groups.get(category).push({ name, description: mod.data.description, level: required });
  }

  if (groups.size === 0) {
    return interaction.editReply({ content: 'No commands are available to you.' });
  }

  // Sort categories by display order, then commands alphabetically within each
  const sortedCategories = [...groups.entries()].sort((a, b) => {
    const aOrder = CATEGORY_LABELS[a[0]]?.order ?? 99;
    const bOrder = CATEGORY_LABELS[b[0]]?.order ?? 99;
    return aOrder - bOrder;
  });

  const embed = new EmbedBuilder()
    .setColor(COLORS.PRIMARY)
    .setTitle('📖 Available Commands')
    .setDescription(`Your access level: **${Object.keys(PERMISSION_LEVELS).find(k => PERMISSION_LEVELS[k] === userLevel)}**`)
    .setFooter({ text: `${client.commands.size} total commands · use /command for details` })
    .setTimestamp();

  for (const [category, commands] of sortedCategories) {
    const meta  = CATEGORY_LABELS[category] ?? { label: `📦 ${category}` };
    commands.sort((a, b) => a.name.localeCompare(b.name));

    const lines = commands.map((cmd) => {
      const badge = userLevel > cmd.level ? LEVEL_BADGE[cmd.level] : '';
      return `\`/${cmd.name}\`${badge} — ${cmd.description}`;
    });

    embed.addFields({ name: meta.label, value: lines.join('\n') });
  }

  await interaction.editReply({ embeds: [embed] });
}
