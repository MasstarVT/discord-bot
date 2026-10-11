// /inventory [user] (pkg-economy.md §5.8, MEE6 /items): the items a member owns on THIS server.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { ITEM_TYPE_LABELS, getInventory, itemLabel } from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const PER_PAGE = 10;

export const data = new SlashCommandBuilder()
  .setName('inventory')
  .setDescription("Show your or a member's items (MEE6 /items).")
  .addUserOption((o) => o.setName('user').setDescription('Member to check (default: you)'));

export async function execute(interaction) {
  const target = interaction.options.getUser('user') ?? interaction.user;
  if (target.bot) {
    return interaction.reply({ embeds: [errorEmbed('Not Allowed', "Bots can't hold coins or items.")], ephemeral: true });
  }
  await interaction.deferReply();

  const { guildId } = interaction;
  const settings = await getSettings(guildId);
  const self = target.id === interaction.user.id;
  const name = clampText(escapeMarkdown(
    (self ? interaction.member?.displayName : interaction.options.getMember('user')?.displayName) ?? target.displayName ?? target.username,
  ), 100);
  const title = `🎒 ${name}'s inventory`;
  const rows = await getInventory(guildId, target.id);

  if (rows.length === 0) {
    return interaction.editReply({
      embeds: [guildEmbed(settings, title, self ? 'You have no items. Browse the shop with `/shop view`.' : `${name} has no items.`)],
    });
  }

  const lines = rows.map((row) =>
    `${itemLabel(row.item)} ×${formatNumber(row.quantity)} · ${ITEM_TYPE_LABELS[row.item.type]}${row.item.enabled ? '' : ' · *retired*'}`);
  const footer = `${rows.length} item type${rows.length === 1 ? '' : 's'}`;
  const pages = [];
  for (let i = 0; i < lines.length; i += PER_PAGE) {
    pages.push(guildEmbed(settings, title, lines.slice(i, i + PER_PAGE).join('\n'), footer));
  }
  return paginate(interaction, pages, { footer: 'append' });
}
