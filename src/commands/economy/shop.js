// /shop view | buy | info (pkg-economy.md §5.7, MEE6 /shop /buy). Purchases are ONE database transaction (stock, price, limit,
// inventory), so two buyers can never take the last copy and nobody can spend coins twice. A role that cannot be granted
// after the commit refunds the purchase. Emits the internal hook `economyPurchase` after a successful purchase.
import { SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { runHooks } from '../../handlers/hookRegistry.js';
import {
  DEFAULT_BUY_MESSAGE, DEFAULT_USE_MESSAGE, ITEM_TYPE_LABELS, EconomyError, economyConfig, effectiveMaxOwned, formatCoins,
  getOwnedQuantity, goesToInventory, itemLabel, itemVars, listItems, peekAccount, purchaseItem, refundPurchase, replyFail,
  resolveItem, searchItems,
} from '../../services/economyService.js';
import { getSettings, getSettingsSafe } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import { guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { checkAssignableRole, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { renderPlaceholders } from '../../utils/placeholderParser.js';
import { clampText, formatNumber } from '../../utils/text.js';

export const module = 'economy';
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

const PER_PAGE = 6;

export const data = new SlashCommandBuilder()
  .setName('shop')
  .setDescription("Browse and buy items from this server's shop (MEE6 /shop /buy).")
  .addSubcommand((s) => s.setName('view').setDescription('List the items for sale')
    .addIntegerOption((o) => o.setName('page').setDescription('Page to open').setMinValue(1).setMaxValue(50)))
  .addSubcommand((s) => s.setName('buy').setDescription('Buy an item (MEE6 /buy)')
    .addStringOption((o) => o.setName('item').setDescription('Item to buy').setRequired(true).setAutocomplete(true).setMaxLength(100))
    .addIntegerOption((o) => o.setName('quantity').setDescription('How many to buy (default 1)').setMinValue(1).setMaxValue(200)))
  .addSubcommand((s) => s.setName('info').setDescription('Show details about an item')
    .addStringOption((o) => o.setName('item').setDescription('Item to show').setRequired(true).setAutocomplete(true).setMaxLength(100)));

// ── Autocomplete ──────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  try {
    const focused = String(interaction.options.getFocused() ?? '');
    const budget = ackBudgetMs(interaction);
    const [rows, settings] = await withTimeout(
      Promise.all([searchItems(interaction.guildId, focused, { enabledOnly: true }), getSettingsSafe(interaction.guildId)]),
      budget, 'shop autocomplete',
    );
    const currency = economyConfig(settings).currencyName;
    await respondAutocomplete(interaction, rows.map((row) => ({ name: `${row.name} — ${formatNumber(row.price)} ${currency}`, value: row.id })));
  } catch (err) {
    logger.debug(`shop autocomplete failed: ${err?.message}`);
    await respondAutocomplete(interaction, []);
  }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const notFound = (interaction, value) => replyFail(interaction, 'Item Not Found',
  `There is no item called **${clampText(escapeMarkdown(value), 100)}** in the shop.`);

function stockText(item) {
  if (item.stock === null) return 'Unlimited';
  return item.stock === 0 ? 'Sold out' : formatNumber(item.stock);
}

function limitText(item) {
  return item.canBuyMultiple ? `${effectiveMaxOwned(item)} per member` : '1 per member';
}

const messageCtx = (interaction, vars) => ({
  member: interaction.member,
  user: interaction.user,
  guild: interaction.guild,
  channel: interaction.channel ?? { id: interaction.channelId },
  vars,
});

// ── /shop view ────────────────────────────────────────────────────────────────

async function view(interaction, settings) {
  const { guildId, user } = interaction;
  const cfg = economyConfig(settings);
  const items = await listItems(guildId, { enabledOnly: true });
  if (items.length === 0) {
    return interaction.editReply({
      embeds: [guildEmbed(settings, '🛒 Shop', 'The shop is empty. Admins can add items with `/economy-admin item create`.')],
    });
  }
  const acc = await peekAccount(guildId, user.id, settings); // viewing never creates a row
  const description = `Buy with \`/shop buy\`. You have ${formatCoins(acc.balance, settings)}.`;
  const pages = [];
  for (let i = 0; i < items.length; i += PER_PAGE) {
    const embed = guildEmbed(settings, '🛒 Shop', description);
    for (const item of items.slice(i, i + PER_PAGE)) {
      const meta = [ITEM_TYPE_LABELS[item.type]];
      if (item.type === 'ROLE' && item.roleId) meta.push(`gives <@&${item.roleId}>`);
      if (item.stock !== null) meta.push(item.stock === 0 ? '**Sold out**' : `${formatNumber(item.stock)} left`);
      if (!item.canBuyMultiple) meta.push('one per member');
      embed.addFields({
        name: `${clampText(item.name, 200)} — ${cfg.currencyEmoji} ${formatNumber(item.price)}`,
        value: clampText(`${clampText(item.description ?? '*No description*', 300)}\n${meta.join(' · ')}`, 1024),
      });
    }
    pages.push(embed);
  }
  const startPage = Math.min((interaction.options.getInteger('page') ?? 1) - 1, pages.length - 1);
  return paginate(interaction, pages, { startPage, footer: 'append' });
}

// ── /shop info ────────────────────────────────────────────────────────────────

async function info(interaction, settings) {
  const value = interaction.options.getString('item', true);
  const item = await resolveItem(interaction.guildId, value, { enabledOnly: true });
  if (!item) return notFound(interaction, value);
  const owned = await getOwnedQuantity(interaction.guildId, interaction.user.id, item.id);
  const embed = guildEmbed(settings, clampText(item.name, 240), item.description ?? null);
  if (item.imageUrl && isHttpUrl(item.imageUrl)) embed.setThumbnail(item.imageUrl);
  embed.addFields(
    { name: 'Price', value: formatCoins(item.price, settings), inline: true },
    { name: 'Type', value: ITEM_TYPE_LABELS[item.type], inline: true },
  );
  if (item.type === 'ROLE') embed.addFields({ name: 'Role', value: item.roleId ? `<@&${item.roleId}>` : '—', inline: true });
  embed.addFields(
    { name: 'Stock', value: stockText(item), inline: true },
    { name: 'Limit', value: limitText(item), inline: true },
  );
  if (item.type !== 'STATIC') {
    embed.addFields(
      { name: 'Use on buy', value: item.useOnBuy ? 'Yes' : 'No', inline: true },
      { name: 'Destroyed on use', value: item.destroyOnUse ? 'Yes' : 'No', inline: true },
    );
  }
  embed.addFields({ name: 'You own', value: formatNumber(owned), inline: true });
  return interaction.editReply({ embeds: [embed] });
}

// ── /shop buy ─────────────────────────────────────────────────────────────────

/** @returns {Promise<boolean>} true when a role item is fine to hand out (otherwise a private error was shown) */
async function checkRoleItem(interaction, item) {
  const role = interaction.guild.roles.cache.get(item.roleId);
  if (!role) {
    await replyFail(interaction, 'Item Broken', "This item's role no longer exists. Ask an admin to fix the item.");
    return false;
  }
  const chk = checkAssignableRole(interaction.guild, role);
  if (!chk.ok) {
    await replyFail(interaction, 'Item Broken', `I can't give this item's role: ${chk.reason}`);
    return false;
  }
  if (interaction.member.roles.cache.has(role.id)) {
    await replyFail(interaction, 'Already Owned', `You already have the ${role} role.`);
    return false;
  }
  return true;
}

const limitReached = (interaction, item, max, owned) => replyFail(interaction, 'Limit Reached',
  `You can own at most ${max} ${itemLabel(item)}${owned ? ` (you have ${owned})` : ''}.`);

async function buy(interaction, settings) {
  const { guildId, user } = interaction;
  const quantity = interaction.options.getInteger('quantity') ?? 1;
  const value = interaction.options.getString('item', true);
  const item = await resolveItem(guildId, value, { enabledOnly: true });
  if (!item) return notFound(interaction, value);

  // Pre-checks are for a friendly message only; the transaction re-checks everything atomically.
  if (!item.canBuyMultiple && quantity > 1) return replyFail(interaction, 'One Per Member', `You can only own one ${itemLabel(item)}.`);
  if (item.type === 'ROLE' && item.useOnBuy && quantity > 1) return replyFail(interaction, 'One At A Time', 'Role items are bought one at a time.');
  if (item.stock !== null && item.stock < quantity) {
    return replyFail(interaction, 'Sold Out', item.stock === 0 ? `${itemLabel(item)} is sold out.` : `Only ${item.stock} left.`);
  }
  if (item.type === 'ROLE' && item.useOnBuy && !(await checkRoleItem(interaction, item))) return undefined;
  if (goesToInventory(item)) {
    const owned = await getOwnedQuantity(guildId, user.id, item.id);
    const max = effectiveMaxOwned(item);
    if (owned + quantity > max) return limitReached(interaction, item, max, owned);
  }

  let res;
  try {
    res = await purchaseItem({ guildId, userId: user.id, itemId: item.id, quantity, settings });
  } catch (err) {
    if (!(err instanceof EconomyError)) throw err;
    if (err.code === 'OUT_OF_STOCK' || err.code === 'ITEM_GONE') {
      return replyFail(interaction, 'Not Available', `${itemLabel(item)} is sold out or no longer for sale.`);
    }
    if (err.code === 'INSUFFICIENT') {
      const acc = await peekAccount(guildId, user.id, settings);
      return replyFail(interaction, 'Insufficient Funds',
        `${itemLabel(item)} costs ${formatCoins(err.data.total, settings)} but you only have ${formatCoins(acc.balance, settings)}.`);
    }
    if (err.code === 'MAX_OWNED') return limitReached(interaction, item, err.data.max, await getOwnedQuantity(guildId, user.id, item.id));
    throw err;
  }

  // From here on every decision uses the row that the transaction locked, not the pre-read item.
  const bought = res.item;
  const useOnBuy = bought.useOnBuy && bought.type !== 'STATIC';
  if (bought.type === 'ROLE' && bought.useOnBuy) {
    try {
      await interaction.member.roles.add(bought.roleId, `Bought shop item: ${bought.name}`);
    } catch (err) {
      logger.warn(`Shop: could not grant role ${bought.roleId} for item ${bought.id} in ${guildId}: ${err?.message}`);
      try {
        await refundPurchase({ guildId, userId: user.id, item: bought, quantity, total: res.total, toInventory: res.toInventory, settings });
      } catch {
        return replyFail(interaction, 'Purchase Failed', "I couldn't give you the role and the automatic refund failed. Please contact an admin.");
      }
      return replyFail(interaction, 'Purchase Refunded',
        "I couldn't give you the role, so your coins were refunded. Ask an admin to check my role permissions.");
    }
  }

  const buyVars = itemVars(bought, { quantity, total: res.total, settings });
  let description = renderPlaceholders(bought.buyMessage || DEFAULT_BUY_MESSAGE, messageCtx(interaction, buyVars), { maxLength: 3500 });
  if (useOnBuy) {
    const useVars = itemVars(bought, { quantity: 1, total: bought.price, settings });
    description += `\n\n${renderPlaceholders(bought.useMessage || DEFAULT_USE_MESSAGE, messageCtx(interaction, useVars), { maxLength: 500 })}`;
  }
  const embed = successEmbed('Purchase Complete', description)
    .addFields({ name: 'Balance', value: formatCoins(res.balance, settings), inline: true });
  if (res.owned !== null) embed.addFields({ name: 'You own', value: formatNumber(res.owned), inline: true });
  if (bought.imageUrl && isHttpUrl(bought.imageUrl)) embed.setThumbnail(bought.imageUrl);
  // The purchase is already committed: a failed reply must not stop the hook below.
  await interaction.editReply({ embeds: [embed] }).catch((err) => logger.warn(`Shop: purchase reply failed: ${err?.message}`));

  await runHooks('economyPurchase', {
    client: interaction.client,
    guild: interaction.guild,
    member: interaction.member,
    userId: user.id,
    item: bought,
    quantity,
    totalPrice: res.total,
    channel: interaction.channel ?? null,
  });
  return undefined;
}

// ── Entry point ───────────────────────────────────────────────────────────────

export async function execute(interaction) {
  await interaction.deferReply();
  const settings = await getSettings(interaction.guildId);
  switch (interaction.options.getSubcommand()) {
    case 'view': return view(interaction, settings);
    case 'info': return info(interaction, settings);
    case 'buy': return buy(interaction, settings);
    default: return undefined;
  }
}
