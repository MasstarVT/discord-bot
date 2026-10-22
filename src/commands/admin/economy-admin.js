// /economy-admin (pkg-economy §5.13, MEE6 economy admin): coins (give / remove / set), shop items (create / edit / delete /
// give / remove / list), settings (currency, daily, work, limits, casino channels, show), coin boosts (add / remove / toggle /
// list) and reset. This file is thin:
// option parsing, validation and replies. Every data access goes through economyService.js (accounts, items, inventories,
// reset) or settingsService.js (GuildSettings); nothing here touches Prisma or Redis.
//
// Reply visibility (deliberate): successes and system errors are public; user-input validation errors are private
// (replyFail, which swaps the public deferral for a private follow-up). Every subcommand defers before any database
// access: publicly, except `item delete` and `reset`, which defer ephemerally, ask for confirmation in that private
// message and post their result with followUp.
//
// `boost add | remove | toggle | list` (pkg-economy.md amendments item 5, MEE6 Economy > Boosts): a boost multiplies ONE
// /daily or /work payout by (100 + percent) % for a member who holds its role (or for everyone). The period says how often a
// member may use it: one-time (once ever), daily, weekly or monthly (guild timezone). At most LIMITS.ECONOMY_BOOSTS per server.
// Rewards, /pay, casino winnings and admin grants are never boosted. Replies are public; validation errors are private.
//
// `config channels` (casino game channel list), options `action` (add / remove / clear / list), `channel` and `mode`:
//  - `mode` (allow / deny) can be combined with ANY action, `list` included. When given, it sets
//    GuildSettings.economyGameChannelMode in the SAME updateSettingsWith mutator as the list change, so both are one atomic
//    write. `list` + `mode` only switches the mode (the list is rewritten unchanged); a plain `list` without `mode` just
//    shows the current setting and writes nothing.
//  - The mutator returns { economyGameChannelIds, economyGameChannelMode }; the mode key is included only when `mode` was given.
//  - ALLOW mode: an empty list means every channel; otherwise games run only in the listed channels. DENY mode: games run
//    everywhere except in the listed channels (a thread counts as its parent); an empty list blocks nothing.
//  - Only channels that still exist count (economyService.gameChannelCheck): a list whose channels are all deleted is
//    "everywhere" in both modes. Deleted channels are shown as ~~id~~ (deleted).
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder, escapeMarkdown } from 'discord.js';
import { COIN_MAX, LIMITS, PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import {
  BOOST_PERCENT_MAX, BOOST_PERIOD_LABELS, EconomyError, GAME_CHANNELS_MAX, ITEM_DESCRIPTION_MAX, ITEM_IMAGE_MAX, ITEM_MESSAGE_MAX,
  ITEM_NAME_MAX, ITEM_TYPE_LABELS, MAX_STOCK, PITY_AMOUNT, PITY_THRESHOLD, RANGE_MAX, WORK_COOLDOWN_MAX_MINUTES,
  clampBalancesToCap, countItems, createBoost, credit, deleteBoost, deleteItem, economyConfig, effectiveMaxOwned,
  findItemByNameInsensitive, formatCoins, giveItem, itemLabel, listBoosts, listItems, removeCoins, replyFail, resetEconomy,
  resolveBoost, resolveItem, searchBoosts, searchItems, setBalance, setBoostEnabled, streakMultiplierLabel, takeItem, createItem,
  updateItem, validateBoostName, validateCurrencyEmoji, validateCurrencyName, validateItemName,
} from '../../services/economyService.js';
import { getSettings, updateSettings, updateSettingsWith } from '../../services/settingsService.js';
import { ackBudgetMs, withTimeout } from '../../utils/async.js';
import { isHttpUrl } from '../../utils/customEmbed.js';
import { errorEmbed, guildEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { awaitConfirm, checkAssignableRole, respondAutocomplete } from '../../utils/interactionHelpers.js';
import { newRoleOption, resolveNewOptions } from '../../utils/newTargets.js';
import logger from '../../utils/logger.js';
import { paginate } from '../../utils/paginator.js';
import { clampText, formatNumber, joinWithLimit } from '../../utils/text.js';

export const module = 'economy';
export const gated = false; // works while the module is off, so an admin can set it up first
export const permissionLevel = PERMISSION_LEVELS.ADMIN;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 3800 of Discord's 4000 characters): keep every description short.

const member = (d) => (o) => o.setName('member').setDescription(d).setRequired(true);
const itemOpt = (o) => o.setName('item').setDescription('Shop item').setRequired(true).setAutocomplete(true).setMaxLength(100);
const boostName = (o) => o.setName('name').setDescription('Boost name').setRequired(true).setAutocomplete(true).setMaxLength(32);
const qty = (o) => o.setName('quantity').setDescription('How many copies (default 1)').setMinValue(1).setMaxValue(LIMITS.ITEM_COPIES_PER_USER);
const TYPE_CHOICES = [
  { name: 'Static (collectible)', value: 'STATIC' },
  { name: 'Usable (with /use)', value: 'USABLE' },
  { name: 'Role (gives a role)', value: 'ROLE' },
];

function itemFields(s, { edit }) {
  if (edit) {
    s.addStringOption(itemOpt)
      .addStringOption((o) => o.setName('name').setDescription('New name').setMaxLength(ITEM_NAME_MAX))
      .addIntegerOption((o) => o.setName('price').setDescription('Price in coins').setMinValue(1).setMaxValue(COIN_MAX));
  } else {
    s.addStringOption((o) => o.setName('name').setDescription('Item name, unique on this server').setRequired(true).setMaxLength(ITEM_NAME_MAX))
      .addIntegerOption((o) => o.setName('price').setDescription('Price in coins').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX));
  }
  s.addStringOption((o) => o.setName('type').setDescription(edit ? 'Item type' : 'Item type (default Static)').addChoices(...TYPE_CHOICES))
    .addRoleOption((o) => o.setName('role').setDescription('Role a Role item gives'))
    .addStringOption((o) => newRoleOption(o))
    .addStringOption((o) => o.setName('description').setDescription('Shown in the shop ("none" clears)').setMaxLength(ITEM_DESCRIPTION_MAX))
    .addStringOption((o) => o.setName('image').setDescription('Image URL, http or https ("none" clears)').setMaxLength(ITEM_IMAGE_MAX))
    .addBooleanOption((o) => o.setName('use_on_buy').setDescription('Use the item as soon as it is bought'))
    .addBooleanOption((o) => o.setName('destroy_on_use').setDescription('Remove one copy each time it is used (default true)'))
    .addBooleanOption((o) => o.setName('can_buy_multiple').setDescription('Members may own more than one (default true)'))
    .addIntegerOption((o) => o.setName('max_per_user').setDescription(`Most copies one member may own (default ${LIMITS.ITEM_COPIES_PER_USER})`)
      .setMinValue(1).setMaxValue(LIMITS.ITEM_COPIES_PER_USER))
    .addIntegerOption((o) => o.setName('stock').setDescription(edit ? 'Copies left for sale (0 = sold out)' : 'Copies for sale (default unlimited)')
      .setMinValue(edit ? 0 : 1).setMaxValue(MAX_STOCK))
    .addStringOption((o) => o.setName('buy_message').setDescription('Shown after buying; {user} {item} {coins} {currency} ("none" clears)').setMaxLength(ITEM_MESSAGE_MAX))
    .addStringOption((o) => o.setName('use_message').setDescription('Shown after using; same variables ("none" clears)').setMaxLength(ITEM_MESSAGE_MAX));
  if (edit) {
    s.addBooleanOption((o) => o.setName('unlimited_stock').setDescription('Remove the stock limit'))
      .addBooleanOption((o) => o.setName('enabled').setDescription('Show the item in the shop'));
  }
  return s;
}

export const data = new SlashCommandBuilder()
  .setName('economy-admin')
  .setDescription('Manage the economy: coins, shop items, settings and reset (MEE6 economy admin).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommandGroup((g) => g.setName('coins').setDescription("Change a member's coins (MEE6 /give-coins /remove-coins)")
    .addSubcommand((s) => s.setName('give').setDescription('Add coins to a member')
      .addUserOption(member('Member to give coins to'))
      .addIntegerOption((o) => o.setName('amount').setDescription('Coins to add').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX)))
    .addSubcommand((s) => s.setName('remove').setDescription('Remove coins from a member (never below 0)')
      .addUserOption(member('Member to take coins from'))
      .addIntegerOption((o) => o.setName('amount').setDescription('Coins to remove').setRequired(true).setMinValue(1).setMaxValue(COIN_MAX)))
    .addSubcommand((s) => s.setName('set').setDescription("Set a member's balance")
      .addUserOption(member('Member to update'))
      .addIntegerOption((o) => o.setName('amount').setDescription('New balance').setRequired(true).setMinValue(0).setMaxValue(COIN_MAX))))
  .addSubcommandGroup((g) => g.setName('item').setDescription('Shop items (MEE6 /give-item /remove-item /spawn-item)')
    .addSubcommand((s) => itemFields(s.setName('create').setDescription('Create a shop item'), { edit: false }))
    .addSubcommand((s) => itemFields(s.setName('edit').setDescription('Edit a shop item'), { edit: true }))
    .addSubcommand((s) => s.setName('delete').setDescription('Delete an item and every copy members own').addStringOption(itemOpt))
    .addSubcommand((s) => s.setName('give').setDescription('Give a member copies of an item (MEE6 /spawn-item)')
      .addUserOption(member('Member to receive the item')).addStringOption(itemOpt).addIntegerOption(qty))
    .addSubcommand((s) => s.setName('remove').setDescription("Take copies of an item from a member's inventory")
      .addUserOption(member('Member to take the item from')).addStringOption(itemOpt).addIntegerOption(qty))
    .addSubcommand((s) => s.setName('list').setDescription('List every item, including disabled ones')))
  .addSubcommandGroup((g) => g.setName('config').setDescription('Economy settings')
    .addSubcommand((s) => s.setName('currency').setDescription('Set the currency name and emoji')
      .addStringOption((o) => o.setName('name').setDescription('Currency name, e.g. coins').setMaxLength(32))
      .addStringOption((o) => o.setName('emoji').setDescription('Currency emoji').setMaxLength(64)))
    .addSubcommand((s) => s.setName('daily').setDescription('Set the /daily base reward range (default 50-120)')
      .addIntegerOption((o) => o.setName('min').setDescription('Lowest base reward').setMinValue(1).setMaxValue(RANGE_MAX))
      .addIntegerOption((o) => o.setName('max').setDescription('Highest base reward').setMinValue(1).setMaxValue(RANGE_MAX)))
    .addSubcommand((s) => s.setName('work').setDescription('Set the /work reward range and cooldown')
      .addIntegerOption((o) => o.setName('min').setDescription('Lowest reward').setMinValue(1).setMaxValue(RANGE_MAX))
      .addIntegerOption((o) => o.setName('max').setDescription('Highest reward').setMinValue(1).setMaxValue(RANGE_MAX))
      .addIntegerOption((o) => o.setName('cooldown').setDescription('Minutes between shifts (default 60)').setMinValue(1).setMaxValue(WORK_COOLDOWN_MAX_MINUTES)))
    .addSubcommand((s) => s.setName('limits').setDescription('Set the starting balance, maximum balance, maximum bet and payout')
      .addIntegerOption((o) => o.setName('starting_balance').setDescription('Balance of a brand-new account').setMinValue(0).setMaxValue(COIN_MAX))
      .addIntegerOption((o) => o.setName('max_balance').setDescription('Most coins one member may hold').setMinValue(1).setMaxValue(COIN_MAX))
      .addIntegerOption((o) => o.setName('max_bet').setDescription('Largest casino bet (0 = no limit)').setMinValue(0).setMaxValue(COIN_MAX))
      .addIntegerOption((o) => o.setName('max_payout').setDescription('Largest net winnings of one casino game (0 = no limit)').setMinValue(0).setMaxValue(COIN_MAX)))
    .addSubcommand((s) => s.setName('channels').setDescription('Choose where casino games can be played (empty list = everywhere)')
      .addStringOption((o) => o.setName('action').setDescription('What to do').setRequired(true).addChoices(
        { name: 'add', value: 'add' }, { name: 'remove', value: 'remove' }, { name: 'clear', value: 'clear' }, { name: 'list', value: 'list' }))
      .addChannelOption((o) => o.setName('channel').setDescription('Channel to add or remove')
        .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildVoice, ChannelType.GuildForum))
      .addStringOption((o) => o.setName('mode').setDescription('allow: only listed channels; deny: all except listed').addChoices(
        { name: 'allow', value: 'allow' }, { name: 'deny', value: 'deny' })))
    .addSubcommand((s) => s.setName('show').setDescription('Show the current economy settings')))
  .addSubcommandGroup((g) => g.setName('boost').setDescription('Coin boosts for /daily and /work (MEE6 boosts, up to 10).')
    .addSubcommand((s) => s.setName('add').setDescription('Create a coin boost')
      .addStringOption((o) => o.setName('name').setDescription('Boost name (unique)').setRequired(true).setMaxLength(32))
      .addIntegerOption((o) => o.setName('percent').setDescription('Extra coins in percent (1-500)').setRequired(true).setMinValue(1).setMaxValue(BOOST_PERCENT_MAX))
      .addStringOption((o) => o.setName('period').setDescription('How often a member can use it').setRequired(true).addChoices(
        { name: 'One-time', value: 'one-time' }, { name: 'Daily', value: 'daily' }, { name: 'Weekly', value: 'weekly' }, { name: 'Monthly', value: 'monthly' }))
      .addRoleOption((o) => o.setName('role').setDescription('Only members with this role (default: everyone)')))
    .addSubcommand((s) => s.setName('remove').setDescription('Delete a boost').addStringOption(boostName))
    .addSubcommand((s) => s.setName('toggle').setDescription('Turn a boost on or off').addStringOption(boostName))
    .addSubcommand((s) => s.setName('list').setDescription('List the boosts')))
  .addSubcommand((s) => s.setName('reset').setDescription('Reset coins and/or items for everyone or one member')
    .addStringOption((o) => o.setName('what').setDescription('What to reset').setRequired(true).addChoices(
      { name: 'Coins', value: 'coins' }, { name: 'Items', value: 'items' }, { name: 'Coins and items', value: 'all' }))
    .addUserOption((o) => o.setName('member').setDescription('Only this member (default everyone)')));

// ── Helpers ───────────────────────────────────────────────────────────────────

const MODE_VALUES = Object.freeze({ allow: 'ALLOW', deny: 'DENY' });
const STREAK_TABLE = [1, 2, 3, 4, 5, 6, 7].map((day) => `Day ${day}${day === 7 ? '+' : ''} ${streakMultiplierLabel(day)}`).join(' · ');

/** A validation error thrown inside the casino-channel mutator (a DB failure is not one). */
class ChannelListError extends Error {}

const label = (text) => clampText(escapeMarkdown(String(text ?? '')), 100);
const nameOf = (user) => `**${label(user.displayName ?? user.username)}**`;
const limitText = (n, settings) => (n > 0 ? formatCoins(n, settings) : 'No limit');
const field = (name, value, inline = true) => ({ name, value: clampText(value, 1024) || '—', inline });

/** `none` (any case) or an empty value clears a text field: null. Absent option: undefined. */
function textOption(options, name) {
  const raw = options.getString(name);
  if (raw === null) return undefined;
  const text = raw.trim();
  return text === '' || text.toLowerCase() === 'none' ? null : text;
}

/** Resolves the `item` option; replies privately and returns null when there is no such item. */
async function findItem(interaction) {
  const value = interaction.options.getString('item', true);
  const item = await resolveItem(interaction.guildId, value);
  if (!item) await replyFail(interaction, 'Item Not Found', `No item called **${label(value)}** exists.`);
  return item;
}

/** The detail fields of an item (the same as /shop info, plus whether it is in the shop). */
function itemDetails(item, settings) {
  const fields = [
    field('Price', formatCoins(item.price, settings)),
    field('Type', ITEM_TYPE_LABELS[item.type] ?? item.type),
  ];
  if (item.type === 'ROLE') fields.push(field('Role', item.roleId ? `<@&${item.roleId}>` : '—'));
  let stock = formatNumber(item.stock);
  if (item.stock === null) stock = 'Unlimited';
  else if (item.stock === 0) stock = 'Sold out';
  fields.push(field('Stock', stock), field('Limit', `${effectiveMaxOwned(item)} per member`));
  if (item.type !== 'STATIC') {
    fields.push(field('Use on buy', item.useOnBuy ? 'Yes' : 'No'), field('Destroyed on use', item.destroyOnUse ? 'Yes' : 'No'));
  }
  fields.push(field('In the shop', item.enabled ? 'Yes' : 'No'));
  if (item.description) fields.push(field('Description', item.description, false));
  return fields;
}

/** The text of the casino channel list for the current mode (shared by `config channels` and `config show`). */
function channelListText(settings, guild, { max = 3500 } = {}) {
  const cfg = economyConfig(settings);
  const deny = cfg.gameChannelMode === 'DENY';
  const ids = cfg.gameChannelIds;
  if (ids.length === 0) {
    return deny ? 'No channels are blocked: casino games can be played everywhere.' : 'Casino games can be played in every channel.';
  }
  const exists = (id) => guild.channels.cache.has(id);
  const list = joinWithLimit(ids.map((id) => (exists(id) ? `<#${id}>` : `~~${id}~~ (deleted)`)), { max });
  let text = `${deny ? 'Casino games are blocked in' : 'Casino games are allowed only in'}: ${list}`;
  if (ids.every((id) => !exists(id))) text += '\nAll of these channels are gone, so games are currently allowed everywhere.';
  return text;
}

const rangeText = (min, max) => `${formatNumber(min)}–${formatNumber(max)}`;

// ── coins ─────────────────────────────────────────────────────────────────────

function logCoins(interaction, sub, target, amount) {
  logger.info(`Economy admin coins ${sub} ${amount} for ${target.id} in ${interaction.guildId} by ${interaction.user.id}`);
}

async function coinsGive(interaction, settings) {
  const target = interaction.options.getUser('member', true);
  const amount = interaction.options.getInteger('amount', true);
  const { balance, capped } = await credit(interaction.guildId, target.id, amount, settings);
  logCoins(interaction, 'give', target, amount);
  return interaction.editReply({
    embeds: [successEmbed(
      'Coins Added',
      `Gave ${formatCoins(amount, settings)} to ${target}.\nNew balance: ${formatCoins(balance, settings)}${capped ? ' (server maximum reached)' : ''}`,
    )],
  });
}

async function coinsRemove(interaction, settings) {
  const target = interaction.options.getUser('member', true);
  const amount = interaction.options.getInteger('amount', true);
  const { balance, removed } = await removeCoins(interaction.guildId, target.id, amount, settings);
  logCoins(interaction, 'remove', target, amount);
  let text = `Removed ${formatCoins(removed, settings)} from ${target}.\nNew balance: ${formatCoins(balance, settings)}`;
  if (removed < amount) text += `\n(They only had ${formatCoins(removed, settings)}.)`;
  return interaction.editReply({ embeds: [successEmbed('Coins Removed', text)] });
}

async function coinsSet(interaction, settings) {
  const target = interaction.options.getUser('member', true);
  const amount = interaction.options.getInteger('amount', true);
  const tooHigh = (max) => replyFail(interaction, 'Too High', `The maximum balance on this server is ${formatCoins(max, settings)}.`);
  const { maxBalance } = economyConfig(settings);
  if (amount > maxBalance) return tooHigh(maxBalance);
  try {
    await setBalance(interaction.guildId, target.id, amount, settings);
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'INVALID') return tooHigh(err.data?.max ?? maxBalance);
    throw err;
  }
  logCoins(interaction, 'set', target, amount);
  return interaction.editReply({ embeds: [successEmbed('Balance Set', `${target} now has ${formatCoins(amount, settings)}.`)] });
}

// ── item ──────────────────────────────────────────────────────────────────────

async function itemCreate(interaction, settings) {
  const { guild, guildId, options } = interaction;
  const invalid = (reason) => replyFail(interaction, 'Invalid Item', reason);

  // 1-3. Name, uniqueness, server cap.
  const checked = validateItemName(options.getString('name', true));
  if (!checked.ok) return invalid(checked.reason);
  const name = checked.value;
  if (await findItemByNameInsensitive(guildId, name)) return invalid(`An item named **${label(name)}** already exists.`);
  if (await countItems(guildId) >= LIMITS.SHOP_ITEMS) return invalid(`This server already has ${LIMITS.SHOP_ITEMS} shop items (the maximum).`);

  // 4-5. Type and role.
  const type = options.getString('type') ?? 'STATIC';
  const picked = options.getRole('role');
  const role = picked ? (guild.roles.cache.get(picked.id) ?? picked) : null;
  if (type === 'ROLE') {
    if (!role) return invalid('A Role item needs a role.');
    const chk = checkAssignableRole(guild, role, { executor: interaction.member });
    if (!chk.ok) return invalid(chk.reason);
  } else if (role) {
    return invalid('Only Role items can have a role. Set type to Role as well.');
  }

  // 6. Image.
  const imageUrl = textOption(options, 'image') ?? null;
  if (imageUrl !== null && !isHttpUrl(imageUrl)) return invalid('The image must be an http(s) URL.');

  // 7. Static items cannot be used.
  const useOnBuy = options.getBoolean('use_on_buy');
  if (useOnBuy === true && type === 'STATIC') return invalid("Static items can't be used, so they can't be used on buy.");

  // 8. Text fields.
  const itemData = {
    name,
    price: options.getInteger('price', true),
    type,
    roleId: role?.id ?? null,
    description: textOption(options, 'description') ?? null,
    imageUrl,
    useOnBuy: useOnBuy ?? (type === 'ROLE'),
    destroyOnUse: options.getBoolean('destroy_on_use') ?? true,
    canBuyMultiple: options.getBoolean('can_buy_multiple') ?? (type !== 'ROLE'),
    maxPerUser: options.getInteger('max_per_user') ?? LIMITS.ITEM_COPIES_PER_USER,
    stock: options.getInteger('stock') ?? null,
    buyMessage: textOption(options, 'buy_message') ?? null,
    useMessage: textOption(options, 'use_message') ?? null,
    enabled: true,
  };

  let item;
  try {
    item = await createItem(guildId, itemData);
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'INVALID') {
      return invalid(err.data?.reason === 'limit'
        ? `This server already has ${LIMITS.SHOP_ITEMS} shop items (the maximum).`
        : `An item named **${label(name)}** already exists.`);
    }
    throw err;
  }

  logger.info(`Economy admin item create ${item.id} in ${guildId} by ${interaction.user.id}`);
  const embed = successEmbed('Item Created', `${itemLabel(item)} is now in the shop for ${formatCoins(item.price, settings)}.`)
    .addFields(itemDetails(item, settings));
  if (item.imageUrl) {
    try {
      embed.setThumbnail(item.imageUrl);
    } catch {
      // not a URL discord.js accepts: the item is created anyway
    }
  }
  return interaction.editReply({ embeds: [embed] });
}

const ITEM_EDIT_OPTIONS = [
  'name', 'price', 'type', 'role', 'description', 'image', 'use_on_buy', 'destroy_on_use', 'can_buy_multiple',
  'max_per_user', 'stock', 'buy_message', 'use_message', 'unlimited_stock', 'enabled',
];

async function itemEdit(interaction) {
  const { guild, guildId, options } = interaction;
  const invalid = (reason) => replyFail(interaction, 'Invalid Item', reason);
  const item = await findItem(interaction);
  if (!item) return undefined;

  // `unlimited_stock: false` asks for nothing, so it does not count as a change.
  const asked = ITEM_EDIT_OPTIONS.some((n) => options.get(n) !== null && !(n === 'unlimited_stock' && options.getBoolean(n) === false));
  if (!asked) return replyFail(interaction, 'Nothing To Change', 'Pass at least one option to change.');

  const data = {};
  const changed = [];
  const set = (option, key, value) => {
    data[key] = value;
    changed.push(option);
  };

  // name
  const newName = options.getString('name');
  if (newName !== null) {
    const checked = validateItemName(newName);
    if (!checked.ok) return invalid(checked.reason);
    if (await findItemByNameInsensitive(guildId, checked.value, { excludeId: item.id })) {
      return invalid(`An item named **${label(checked.value)}** already exists.`);
    }
    set('name', 'name', checked.value);
  }

  // type and role
  const newType = options.getString('type');
  const effectiveType = newType ?? item.type;
  const picked = options.getRole('role');
  const role = picked ? (guild.roles.cache.get(picked.id) ?? picked) : null;
  if (role && effectiveType !== 'ROLE') return invalid('Only Role items can have a role. Set type to Role as well.');
  if (effectiveType === 'ROLE') {
    if (role) {
      const chk = checkAssignableRole(guild, role, { executor: interaction.member });
      if (!chk.ok) return invalid(chk.reason);
      set('role', 'roleId', role.id);
    } else if (newType === 'ROLE' && !item.roleId) {
      return invalid('A Role item needs a role.');
    }
  } else if (item.type === 'ROLE') {
    data.roleId = null; // changing away from Role
  }
  if (newType !== null) set('type', 'type', newType);

  // price and the other scalars
  const price = options.getInteger('price');
  if (price !== null) set('price', 'price', price);
  const maxPerUser = options.getInteger('max_per_user');
  if (maxPerUser !== null) set('max_per_user', 'maxPerUser', maxPerUser);
  for (const [option, key] of [['destroy_on_use', 'destroyOnUse'], ['can_buy_multiple', 'canBuyMultiple'], ['enabled', 'enabled']]) {
    const value = options.getBoolean(option);
    if (value !== null) set(option, key, value);
  }

  // use on buy: a Static item cannot be used
  const useOnBuy = options.getBoolean('use_on_buy');
  if (effectiveType === 'STATIC' && (useOnBuy === true || (newType === 'STATIC' && useOnBuy === null && item.useOnBuy))) {
    return invalid(`Static items can't be used, so they can't be used on buy.${useOnBuy === null ? ' Set use_on_buy to false as well.' : ''}`);
  }
  if (useOnBuy !== null) set('use_on_buy', 'useOnBuy', useOnBuy);

  // image and texts: "none" clears
  const image = textOption(options, 'image');
  if (image !== undefined) {
    if (image !== null && !isHttpUrl(image)) return invalid('The image must be an http(s) URL.');
    set('image', 'imageUrl', image);
  }
  for (const [option, key] of [['description', 'description'], ['buy_message', 'buyMessage'], ['use_message', 'useMessage']]) {
    const value = textOption(options, option);
    if (value !== undefined) set(option, key, value);
  }

  // stock
  const stock = options.getInteger('stock');
  const unlimited = options.getBoolean('unlimited_stock');
  if (stock !== null && unlimited === true) return invalid('Use either stock or unlimited_stock, not both.');
  if (unlimited === true) set('unlimited_stock', 'stock', null);
  else if (stock !== null) set('stock', 'stock', stock);

  let updated;
  try {
    updated = await updateItem(guildId, item.id, data);
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'ITEM_GONE') return replyFail(interaction, 'Item Not Found', 'That item was just deleted.');
    if (err instanceof EconomyError && err.code === 'INVALID') return invalid(`An item named **${label(data.name)}** already exists.`);
    throw err;
  }
  return interaction.editReply({
    embeds: [successEmbed('Item Updated', `${itemLabel(updated)} was updated: ${changed.join(', ')}.`)],
  });
}

async function itemDelete(interaction) {
  const item = await findItem(interaction);
  if (!item) return undefined;
  const ok = await awaitConfirm(interaction, {
    title: 'Delete item?',
    description: `Delete ${itemLabel(item)}? Every copy members own is removed too. This cannot be undone.`,
    confirmLabel: 'Delete',
    danger: true,
    ephemeral: true,
  });
  if (!ok) return undefined;
  const { copies } = await deleteItem(interaction.guildId, item.id);
  logger.info(`Economy admin item delete ${item.id} (${copies} copies) in ${interaction.guildId} by ${interaction.user.id}`);
  const owned = copies ? ` (${formatNumber(copies)} owned cop${copies === 1 ? 'y' : 'ies'} removed)` : '';
  return interaction.followUp({ embeds: [successEmbed('Item Deleted', `${itemLabel(item)} was deleted${owned}.`)] });
}

async function itemGive(interaction) {
  const target = interaction.options.getUser('member', true);
  const quantity = interaction.options.getInteger('quantity') ?? 1;
  const item = await findItem(interaction);
  if (!item) return undefined;
  try {
    const { owned } = await giveItem({ guildId: interaction.guildId, userId: target.id, itemId: item.id, quantity });
    logger.info(`Economy admin item give ${quantity}x ${item.id} to ${target.id} in ${interaction.guildId} by ${interaction.user.id}`);
    return interaction.editReply({
      embeds: [successEmbed('Item Given', `Gave ${quantity}× ${itemLabel(item)} to ${target}. They now have ${owned}.`)],
    });
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'MAX_OWNED') {
      return replyFail(interaction, 'Limit Reached', `${target} can hold at most ${LIMITS.ITEM_COPIES_PER_USER} copies (they have ${err.data?.owned ?? 0}).`);
    }
    if (err instanceof EconomyError && err.code === 'ITEM_GONE') return replyFail(interaction, 'Item Not Found', 'That item was just deleted.');
    throw err;
  }
}

async function itemRemove(interaction) {
  const target = interaction.options.getUser('member', true);
  const quantity = interaction.options.getInteger('quantity') ?? 1;
  const item = await findItem(interaction);
  if (!item) return undefined;
  try {
    const { removed, remaining } = await takeItem({ guildId: interaction.guildId, userId: target.id, itemId: item.id, quantity });
    logger.info(`Economy admin item remove ${removed}x ${item.id} from ${target.id} in ${interaction.guildId} by ${interaction.user.id}`);
    return interaction.editReply({
      embeds: [successEmbed('Item Removed', `Removed ${removed}× ${itemLabel(item)} from ${target}. They have ${remaining} left.`)],
    });
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'NOT_OWNED') {
      return replyFail(interaction, 'Not Owned', `${target} doesn't have ${itemLabel(item)}.`);
    }
    throw err;
  }
}

async function itemList(interaction, settings) {
  const items = await listItems(interaction.guildId, { enabledOnly: false, sort: 'name' });
  if (items.length === 0) {
    return interaction.editReply({ embeds: [guildEmbed(settings, 'Shop Items', 'No items yet. Create one with `/economy-admin item create`.')] });
  }
  const lines = items.map((i) => {
    const stock = i.stock !== null ? ` · stock ${formatNumber(i.stock)}` : '';
    return `${itemLabel(i)} · ${formatCoins(i.price, settings, { bold: false })} · ${ITEM_TYPE_LABELS[i.type] ?? i.type}${stock}${i.enabled ? '' : ' · ⛔ disabled'}`;
  });
  const footer = `${items.length}/${LIMITS.SHOP_ITEMS} items`;
  const pages = [];
  for (let i = 0; i < lines.length; i += 10) pages.push(guildEmbed(settings, 'Shop Items', lines.slice(i, i + 10).join('\n'), footer));
  return paginate(interaction, pages);
}

// ── config ────────────────────────────────────────────────────────────────────

async function configCurrency(interaction, settings) {
  const { options, guild, guildId } = interaction;
  const nameOpt = options.getString('name');
  const emojiOpt = options.getString('emoji');
  if (nameOpt === null && emojiOpt === null) {
    const cfg = economyConfig(settings);
    return interaction.editReply({
      embeds: [guildEmbed(settings, 'Economy Currency', `Currency: ${cfg.currencyEmoji} **${cfg.currencyName}**\nPreview: ${formatCoins(1234, settings)}`)],
    });
  }
  const data = {};
  let note = '';
  if (nameOpt !== null) {
    const checked = validateCurrencyName(nameOpt);
    if (!checked.ok) return replyFail(interaction, 'Invalid Currency', checked.reason);
    data.currencyName = checked.value;
  }
  if (emojiOpt !== null) {
    const checked = validateCurrencyEmoji(emojiOpt);
    if (!checked.ok) return replyFail(interaction, 'Invalid Currency', checked.reason);
    data.currencyEmoji = checked.value;
    if (checked.customId && !guild.emojis.cache.has(checked.customId)) {
      note = '\nNote: this emoji is from another server; I can only show it if I am in that server.';
    }
  }
  const updated = await updateSettings(guildId, data);
  const cfg = economyConfig(updated);
  return interaction.editReply({
    embeds: [successEmbed('Economy Updated', `Currency: ${cfg.currencyEmoji} **${cfg.currencyName}**\nPreview: ${formatCoins(1234, updated)}${note}`)],
  });
}

async function configDaily(interaction, settings) {
  const { options, guildId } = interaction;
  const min = options.getInteger('min');
  const max = options.getInteger('max');
  const cfg = economyConfig(settings);
  if (min === null && max === null) {
    return interaction.editReply({
      embeds: [guildEmbed(settings, 'Daily Reward', `Base reward: ${rangeText(cfg.dailyMin, cfg.dailyMax)}\n${STREAK_TABLE}`)],
    });
  }
  const newMin = min ?? cfg.dailyMin;
  const newMax = max ?? cfg.dailyMax;
  if (newMin > newMax) return replyFail(interaction, 'Invalid Range', 'The minimum must not be greater than the maximum.');
  await updateSettings(guildId, { dailyMin: newMin, dailyMax: newMax });
  return interaction.editReply({
    embeds: [successEmbed('Economy Updated', `The /daily base reward is now ${rangeText(newMin, newMax)}.\n${STREAK_TABLE}`)],
  });
}

async function configWork(interaction, settings) {
  const { options, guildId } = interaction;
  const min = options.getInteger('min');
  const max = options.getInteger('max');
  const cooldown = options.getInteger('cooldown');
  const cfg = economyConfig(settings);
  if (min === null && max === null && cooldown === null) {
    return interaction.editReply({
      embeds: [guildEmbed(settings, 'Work Reward', `Reward: ${rangeText(cfg.workMin, cfg.workMax)} every ${formatNumber(cfg.workCooldownMinutes)} min`)],
    });
  }
  const newMin = min ?? cfg.workMin;
  const newMax = max ?? cfg.workMax;
  if (newMin > newMax) return replyFail(interaction, 'Invalid Range', 'The minimum must not be greater than the maximum.');
  const data = {};
  if (min !== null || max !== null) Object.assign(data, { workMin: newMin, workMax: newMax });
  if (cooldown !== null) data.workCooldownMinutes = cooldown;
  const updated = await updateSettings(guildId, data);
  const now = economyConfig(updated);
  return interaction.editReply({
    embeds: [successEmbed('Economy Updated', `/work now pays ${rangeText(now.workMin, now.workMax)} every ${formatNumber(now.workCooldownMinutes)} min.`)],
  });
}

function limitsSummary(settings) {
  const cfg = economyConfig(settings);
  return [
    `Starting balance: ${formatCoins(cfg.startingBalance, settings)}`,
    `Maximum balance: ${formatCoins(cfg.maxBalance, settings)}`,
    `Maximum bet: ${limitText(cfg.maxBet, settings)}`,
    `Maximum payout: ${limitText(cfg.maxPayout, settings)}`,
  ].join('\n');
}

async function configLimits(interaction, settings) {
  const { options, guildId } = interaction;
  const start = options.getInteger('starting_balance');
  const maxBalance = options.getInteger('max_balance');
  const maxBet = options.getInteger('max_bet');
  const maxPayout = options.getInteger('max_payout');
  if (start === null && maxBalance === null && maxBet === null && maxPayout === null) {
    return interaction.editReply({ embeds: [guildEmbed(settings, 'Economy Limits', limitsSummary(settings))] });
  }
  const cfg = economyConfig(settings);
  const newMax = maxBalance ?? cfg.maxBalance;
  const newStart = start ?? cfg.startingBalance;
  if (newStart > newMax) return replyFail(interaction, 'Invalid Limits', 'The starting balance cannot be above the maximum balance.');
  const data = {};
  if (start !== null) data.economyStartingBalance = start;
  if (maxBalance !== null) data.economyMaxBalance = maxBalance;
  if (maxBet !== null) data.economyMaxBet = maxBet;
  if (maxPayout !== null) data.economyMaxPayout = maxPayout;
  const updated = await updateSettings(guildId, data);
  let text = limitsSummary(updated);
  if (maxBalance !== null && maxBalance < cfg.maxBalance) {
    const n = await clampBalancesToCap(guildId, newMax);
    if (n > 0) text += `\n${formatNumber(n)} balance${n === 1 ? ' was' : 's were'} lowered to the new maximum.`;
  }
  return interaction.editReply({ embeds: [successEmbed('Economy Updated', text)] });
}

async function configChannels(interaction, settings) {
  const { options, guild, guildId } = interaction;
  const action = options.getString('action', true);
  const channel = options.getChannel('channel');
  const mode = MODE_VALUES[options.getString('mode')] ?? null;
  if ((action === 'add' || action === 'remove') && !channel) return replyFail(interaction, 'Missing Channel', 'Pick a channel.');
  if (action === 'list' && mode === null) {
    return interaction.editReply({ embeds: [guildEmbed(settings, 'Casino Channels', channelListText(settings, guild))] });
  }

  let updated;
  try {
    updated = await updateSettingsWith(guildId, (cur) => {
      const ids = [...(cur.economyGameChannelIds ?? [])];
      const deny = (mode ?? cur.economyGameChannelMode) === 'DENY';
      if (action === 'add') {
        if (ids.includes(channel.id)) throw new ChannelListError(`That channel is already ${deny ? 'blocked' : 'allowed'}.`);
        if (ids.length >= GAME_CHANNELS_MAX) throw new ChannelListError(`You can ${deny ? 'block' : 'allow'} at most ${GAME_CHANNELS_MAX} channels.`);
        ids.push(channel.id);
      } else if (action === 'remove') {
        if (!ids.includes(channel.id)) throw new ChannelListError('That channel is not in the list.');
        ids.splice(ids.indexOf(channel.id), 1);
      } else if (action === 'clear') {
        ids.length = 0;
      }
      return { economyGameChannelIds: ids, ...(mode ? { economyGameChannelMode: mode } : {}) };
    });
  } catch (err) {
    if (err instanceof ChannelListError) return replyFail(interaction, 'Not Changed', err.message);
    throw err;
  }

  const modeLine = mode ? `Mode: **${mode === 'DENY' ? 'deny list' : 'allow list'}**\n` : '';
  return interaction.editReply({
    embeds: [successEmbed('Casino Channels Updated', `${modeLine}${channelListText(updated, guild)}`)],
  });
}

async function configShow(interaction, settings) {
  const { guild, guildId } = interaction;
  const cfg = economyConfig(settings);
  const count = await countItems(guildId);
  const deny = cfg.gameChannelMode === 'DENY';
  const embed = guildEmbed(settings, '🪙 Economy Settings', null).addFields(
    field('Module', isModuleEnabled(settings, 'economy') ? '✅ On' : '❌ Off · enable with /module enable economy'),
    field('Currency', `${cfg.currencyEmoji} ${cfg.currencyName}`),
    field('Starting balance', formatCoins(cfg.startingBalance, settings)),
    field('Maximum balance', formatCoins(cfg.maxBalance, settings)),
    field('Maximum bet', limitText(cfg.maxBet, settings)),
    field('Maximum payout', limitText(cfg.maxPayout, settings)),
    field('Daily', `${rangeText(cfg.dailyMin, cfg.dailyMax)} base · streak up to ×4 · 24 h cooldown, streak kept for 48 h`, false),
    field('Work', `${rangeText(cfg.workMin, cfg.workMax)} every ${formatNumber(cfg.workCooldownMinutes)} min`),
    field('Pity', `${formatNumber(PITY_AMOUNT)} below ${formatNumber(PITY_THRESHOLD)}, every 12 h`),
    field('Casino channels', `**${deny ? 'Deny list' : 'Allow list'}**\n${channelListText(settings, guild, { max: 800 })}`, false),
    field('Shop', `${count}/${LIMITS.SHOP_ITEMS} items`),
  );
  return interaction.editReply({ embeds: [embed] });
}

// ── boost ─────────────────────────────────────────────────────────────────────

const BOOST_PER_PAGE = 10;
const PERIOD_CHOICES = Object.freeze({ 'one-time': 'ONE_TIME', daily: 'DAILY', weekly: 'WEEKLY', monthly: 'MONTHLY' });
const PERIOD_TEXT = Object.freeze({
  ONE_TIME: 'once per member, ever',
  DAILY: 'once per member per day',
  WEEKLY: 'once per member per week',
  MONTHLY: 'once per member per month',
});

const boostLabel = (b) => `**${clampText(escapeMarkdown(b.name), 40)}**`;
const boostTarget = (b) => (b.roleId ? `<@&${b.roleId}>` : 'everyone');

async function boostAdd(interaction, settings) {
  const checked = validateBoostName(interaction.options.getString('name', true));
  if (!checked.ok) return replyFail(interaction, 'Invalid Name', checked.reason);
  const period = PERIOD_CHOICES[interaction.options.getString('period', true)];
  const role = interaction.options.getRole('role');
  const roleId = role && role.id !== interaction.guildId ? role.id : null; // @everyone means "everyone"
  const percent = interaction.options.getInteger('percent', true);

  let boost;
  try {
    boost = await createBoost(interaction.guildId, { name: checked.value, percent, period, roleId, createdBy: interaction.user.id });
  } catch (err) {
    if (err instanceof EconomyError && err.code === 'INVALID') {
      if (err.data.reason === 'limit') return replyFail(interaction, 'Too Many Boosts', `This server already has ${LIMITS.ECONOMY_BOOSTS} boosts (the maximum).`);
      if (err.data.reason === 'name-taken') return replyFail(interaction, 'Name Taken', `A boost named ${boostLabel({ name: checked.value })} already exists.`);
    }
    throw err;
  }
  logger.info(`Economy boost "${boost.name}" +${percent}% (${period}) created in ${interaction.guildId} by ${interaction.user.id}`);
  return interaction.editReply({
    embeds: [successEmbed('Boost Added',
      `${boostLabel(boost)} gives ${boostTarget(boost)} **+${boost.percent}%** on one /daily or /work payout, ${PERIOD_TEXT[boost.period]}.`,
      `${settings.economyEnabled ? '' : 'The economy module is off right now. '}Only the highest unused boost applies to a payout.`)],
  });
}

async function boostRemove(interaction) {
  const value = interaction.options.getString('name', true);
  const boost = await resolveBoost(interaction.guildId, value);
  if (!boost || !(await deleteBoost(interaction.guildId, boost.id))) {
    return replyFail(interaction, 'Boost Not Found', `No boost called **${clampText(escapeMarkdown(value), 40)}** exists.`);
  }
  logger.info(`Economy boost "${boost.name}" deleted in ${interaction.guildId} by ${interaction.user.id}`);
  return interaction.editReply({ embeds: [successEmbed('Boost Removed', `${boostLabel(boost)} was deleted.`)] });
}

async function boostToggle(interaction) {
  const value = interaction.options.getString('name', true);
  const boost = await resolveBoost(interaction.guildId, value);
  const updated = boost ? await setBoostEnabled(interaction.guildId, boost.id, !boost.enabled) : null;
  if (!updated) return replyFail(interaction, 'Boost Not Found', `No boost called **${clampText(escapeMarkdown(value), 40)}** exists.`);
  return interaction.editReply({
    embeds: [successEmbed(updated.enabled ? 'Boost Enabled' : 'Boost Disabled', `${boostLabel(updated)} is now ${updated.enabled ? 'on' : 'off'}.`)],
  });
}

async function boostList(interaction, settings) {
  const boosts = await listBoosts(interaction.guildId);
  if (boosts.length === 0) {
    return interaction.editReply({ embeds: [guildEmbed(settings, '🚀 Coin Boosts', 'No boosts yet. Create one with `/economy-admin boost add`.')] });
  }
  const lines = boosts.map((b) =>
    `${boostLabel(b)} · +${b.percent}% · ${BOOST_PERIOD_LABELS[b.period]} · ${boostTarget(b)} · ${b.enabled ? '✅ on' : '⛔ off'} · ${formatNumber(b._count.uses)} use${b._count.uses === 1 ? '' : 's'}`);
  const footer = `${boosts.length}/${LIMITS.ECONOMY_BOOSTS} boosts`;
  const pages = [];
  for (let i = 0; i < lines.length; i += BOOST_PER_PAGE) {
    pages.push(guildEmbed(settings, '🚀 Coin Boosts', lines.slice(i, i + BOOST_PER_PAGE).join('\n'), footer));
  }
  return paginate(interaction, pages, { footer: 'append' });
}

// ── reset ─────────────────────────────────────────────────────────────────────

async function reset(interaction, settings) {
  const { guildId, options } = interaction;
  const what = options.getString('what', true);
  const target = options.getUser('member');
  const { startingBalance } = economyConfig(settings);
  const start = formatCoins(startingBalance, settings);

  const parts = [];
  if (what === 'coins' || what === 'all') {
    parts.push(target
      ? `${nameOf(target)}'s balance becomes ${start} (the starting balance) and their daily streak and cooldowns are cleared.`
      : `Every member's balance becomes ${start} (the starting balance) and daily streaks and cooldowns are cleared. Members who never used the economy here will start at ${start} instead of their old global balance.`);
  }
  if (what === 'items') {
    parts.push(target ? `Every item in ${nameOf(target)}'s inventory is removed.` : 'Every item in every inventory is removed. Shop items stay.');
  } else if (what === 'all') {
    parts.push(target ? `Every item in their inventory is removed.` : 'Every item in every inventory is removed. Shop items stay.');
  }
  const ok = await awaitConfirm(interaction, {
    title: 'Reset economy?',
    description: `${parts.join(' ')} This cannot be undone.`,
    confirmLabel: 'Reset',
    danger: true,
    ephemeral: true,
  });
  if (!ok) return undefined;

  const r = await resetEconomy({ guildId, userId: target?.id ?? null, what, settings });
  const done = [];
  if (what === 'coins' || what === 'all') done.push(`${formatNumber(r.accounts)} balance${r.accounts === 1 ? '' : 's'}`);
  if (what === 'items' || what === 'all') done.push(`${formatNumber(r.inventories)} inventory entr${r.inventories === 1 ? 'y' : 'ies'}`);
  logger.info(`Economy admin reset ${what} (${target ? target.id : 'everyone'}) in ${guildId} by ${interaction.user.id}`);
  return interaction.followUp({ embeds: [successEmbed('Economy Reset', `Reset ${done.join(' and ')}.`)] });
}

// ── Execute ───────────────────────────────────────────────────────────────────

const HANDLERS = {
  'coins give': coinsGive,
  'coins remove': coinsRemove,
  'coins set': coinsSet,
  'item create': itemCreate,
  'item edit': itemEdit,
  'item delete': itemDelete,
  'item give': itemGive,
  'item remove': itemRemove,
  'item list': itemList,
  'config currency': configCurrency,
  'config daily': configDaily,
  'config work': configWork,
  'config limits': configLimits,
  'config channels': configChannels,
  'config show': configShow,
  'boost add': boostAdd,
  'boost remove': boostRemove,
  'boost toggle': boostToggle,
  'boost list': boostList,
  reset,
};
/** These two confirm privately and post the result with followUp; every other subcommand answers publicly. */
const PRIVATE_FLOW = new Set(['item delete', 'reset']);
/** Subcommands with a `member` option that must not be a bot. */
const BOT_CHECKED = new Set(['coins give', 'coins remove', 'coins set', 'item give', 'item remove', 'reset']);

export async function execute(interaction) {
  const group = interaction.options.getSubcommandGroup(false);
  const sub = interaction.options.getSubcommand();
  const key = group ? `${group} ${sub}` : sub;
  const handler = HANDLERS[key];
  if (!handler) {
    await interaction.reply({ embeds: [errorEmbed('Unknown Subcommand', 'This subcommand no longer exists.')], ephemeral: true });
    return;
  }

  // No database needed: a bot can never hold coins or items.
  if (BOT_CHECKED.has(key) && interaction.options.getUser('member')?.bot) {
    await interaction.reply({ embeds: [errorEmbed('Not Allowed', "Bots can't hold coins or items.")], ephemeral: true });
    return;
  }

  // Acknowledge before any settings or database access.
  if (PRIVATE_FLOW.has(key)) await interaction.deferReply({ ephemeral: true });
  else await interaction.deferReply();
  if (!(await resolveNewOptions(interaction, data))) return;
  const settings = await getSettings(interaction.guildId);
  await handler(interaction, settings);
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/** `boost remove | toggle`: the boost names. */
async function boostAutocomplete(interaction) {
  try {
    const rows = await withTimeout(
      searchBoosts(interaction.guildId, String(interaction.options.getFocused() ?? '')),
      ackBudgetMs(interaction), 'economy-admin boost autocomplete',
    );
    await respondAutocomplete(interaction, rows.map((row) => ({
      name: `${row.name} (+${row.percent}%)${row.enabled ? '' : ' (off)'}`,
      value: row.id,
    })));
  } catch (err) {
    logger.debug(`economy-admin boost autocomplete failed: ${err?.message}`);
    await respondAutocomplete(interaction, []);
  }
}

export async function autocomplete(interaction) {
  if (interaction.options.getSubcommandGroup(false) === 'boost') return boostAutocomplete(interaction);
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name !== 'item' || !interaction.guildId) return respondAutocomplete(interaction, []);
    const budget = ackBudgetMs(interaction);
    if (budget <= 0) return respondAutocomplete(interaction, []);
    const query = String(focused.value ?? '').trim().slice(0, 100);
    const rows = await withTimeout(searchItems(interaction.guildId, query), budget, 'economy-admin autocomplete');
    return respondAutocomplete(interaction, rows.map((row) => ({
      name: `${row.name}${row.enabled ? '' : ' (disabled)'} — ${formatNumber(row.price)}`,
      value: row.id,
    })));
  } catch (err) {
    logger.debug(`economy-admin autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
