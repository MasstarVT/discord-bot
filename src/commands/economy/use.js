// /use <item> (pkg-economy.md §5.9, MEE6 /use): uses an item from your inventory. A role item gives its role (hierarchy checked
// first, so nothing is consumed when the role cannot be granted); a failed grant after the item was consumed gives the item back.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  DEFAULT_USE_MESSAGE, EconomyError, consumeItem, getOwnedQuantity, itemLabel, itemVars, replyFail, resolveItem, restoreItem,
  searchInventory,
} from '../../services/economyService.js';
import { getSettings } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import { successEmbed } from '../../utils/embedBuilder.js';
import { checkAssignableRole, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { renderPlaceholders } from '../../utils/placeholderParser.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('use')
  .setDescription('Use an item from your inventory (MEE6 /use).')
  .addStringOption((o) => o.setName('item').setDescription('Item to use').setRequired(true).setAutocomplete(true).setMaxLength(100));

export async function autocomplete(interaction) {
  try {
    const rows = await withTimeout(
      searchInventory(interaction.guildId, interaction.user.id, String(interaction.options.getFocused() ?? '')),
      ackBudgetMs(interaction), 'use autocomplete',
    );
    await respondAutocomplete(interaction, rows.map((row) => ({ name: `${row.item.name} ×${row.quantity}`, value: row.item.id })));
  } catch (err) {
    logger.debug(`use autocomplete failed: ${err?.message}`);
    await respondAutocomplete(interaction, []);
  }
}

export async function execute(interaction) {
  await interaction.deferReply();
  const { guildId, guild, member, user } = interaction;
  const settings = await getSettings(guildId);
  const value = interaction.options.getString('item', true);

  const item = await resolveItem(guildId, value); // a retired (disabled) item can still be used
  if (!item) {
    return replyFail(interaction, 'Item Not Found', `There is no item called **${clampText(escapeMarkdown(value), 100)}** here.`);
  }
  const owned = await getOwnedQuantity(guildId, user.id, item.id);
  if (owned === 0) return replyFail(interaction, 'Not Owned', `You don't have ${itemLabel(item)}.`);
  if (item.type === 'STATIC') return replyFail(interaction, 'Not Usable', `${itemLabel(item)} is a collectible and can't be used.`);

  // Role items: validate before anything is consumed.
  let role = null;
  if (item.type === 'ROLE') {
    role = guild.roles.cache.get(item.roleId);
    if (!role) return replyFail(interaction, 'Item Broken', "This item's role no longer exists. Ask an admin to fix the item.");
    const chk = checkAssignableRole(guild, role);
    if (!chk.ok) return replyFail(interaction, 'Item Broken', `I can't give this item's role: ${chk.reason}`);
    if (member.roles.cache.has(role.id)) return replyFail(interaction, 'Already Owned', `You already have the ${role} role.`);
  }

  let remaining = owned;
  if (item.destroyOnUse) {
    try {
      remaining = (await consumeItem({ guildId, userId: user.id, itemId: item.id })).remaining;
    } catch (err) {
      if (err instanceof EconomyError && err.code === 'NOT_OWNED') return replyFail(interaction, 'Not Owned', `You don't have ${itemLabel(item)}.`);
      throw err;
    }
  }

  if (role) {
    try {
      await member.roles.add(role.id, `Used item: ${item.name}`);
    } catch (err) {
      logger.warn(`Use: could not grant role ${role.id} for item ${item.id} in ${guildId}: ${err?.message}`);
      if (item.destroyOnUse) await restoreItem({ guildId, userId: user.id, itemId: item.id });
      return replyFail(interaction, 'Could Not Use', "I couldn't give you the role, so your item was returned.");
    }
  }

  const description = renderPlaceholders(item.useMessage || DEFAULT_USE_MESSAGE, {
    member, user, guild, channel: interaction.channel ?? { id: interaction.channelId },
    vars: itemVars(item, { quantity: 1, settings }),
  }, { maxLength: 4000 });
  const embed = successEmbed('Item Used', description).addFields({ name: 'Left', value: formatNumber(remaining), inline: true });
  if (item.imageUrl && isHttpUrl(item.imageUrl)) embed.setThumbnail(item.imageUrl);
  return interaction.editReply({ embeds: [embed] });
}
