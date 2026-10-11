// Database access for the `shopItems` collection (the ShopItem table). Writes go through economyService.js
// (createItem, updateItem, deleteItem), which keep the per-server cap and the case-insensitive name unique under a lock on
// the GuildSettings row, exactly like /economy-admin item. Shop items are not cached anywhere.
import prisma from '../../database/client.js';
import { EconomyError, createItem, deleteItem, listItems, updateItem, validateItemName } from '../../services/economyService.js';
import { LIMITS } from '../../config/constants.js';
import { RecordError } from './errors.js';
import { collection } from './shopItems.js';

const TYPE_TEXT = Object.freeze({ STATIC: 'collectible', USABLE: 'usable', ROLE: 'role' });

const summaryOf = (row) => [
  row.price.toLocaleString('en-US'),
  TYPE_TEXT[row.type] ?? row.type,
  row.stock === null || row.stock === undefined ? null : (row.stock === 0 ? 'sold out' : `stock ${row.stock.toLocaleString('en-US')}`),
  row.enabled === false ? 'hidden' : null,
].filter(Boolean).join(' · ');

const toItem = (row) => ({
  id: row.id,
  name: row.name,
  price: row.price,
  type: row.type,
  roleId: row.roleId ?? null,
  description: row.description ?? null,
  imageUrl: row.imageUrl ?? null,
  useOnBuy: row.useOnBuy === true,
  destroyOnUse: row.destroyOnUse !== false,
  canBuyMultiple: row.canBuyMultiple !== false,
  maxPerUser: row.maxPerUser,
  stock: row.stock ?? null,
  buyMessage: row.buyMessage ?? null,
  useMessage: row.useMessage ?? null,
  enabled: row.enabled !== false,
  summary: summaryOf(row),
  createdAt: row.createdAt instanceof Date ? row.createdAt.toISOString() : null,
});

const checkName = (name) => {
  const checked = validateItemName(name);
  if (!checked.ok) throw new RecordError('invalid', checked.reason, { path: 'name' });
  return checked.value;
};

/** Maps the service's EconomyError to the error the admin sees. */
function mapError(err, name) {
  if (!(err instanceof EconomyError)) return err;
  if (err.code === 'ITEM_GONE') return new RecordError('not_found', 'That item no longer exists.');
  if (err.code === 'INVALID' && err.data?.reason === 'limit') {
    return new RecordError('limit', `This server already has ${LIMITS.SHOP_ITEMS} shop items (the maximum).`);
  }
  if (err.code === 'INVALID') return new RecordError('conflict', `An item named "${name}" already exists.`, { path: 'name' });
  return err;
}

export const store = Object.freeze({
  async list(guildId) {
    return (await listItems(guildId, { enabledOnly: false, sort: 'name' })).map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.shopItem.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx = {}) {
    const name = checkName(data.name);
    const itemData = {
      name,
      price: data.price,
      type: data.type,
      roleId: data.type === 'ROLE' ? data.roleId : null,
      description: data.description,
      imageUrl: data.imageUrl,
      useOnBuy: data.useOnBuy,
      destroyOnUse: data.destroyOnUse,
      canBuyMultiple: data.canBuyMultiple,
      maxPerUser: data.maxPerUser,
      stock: data.stock,
      buyMessage: data.buyMessage,
      useMessage: data.useMessage,
      enabled: data.enabled,
    };
    try {
      const row = await createItem(guildId, itemData);
      ctx.logger?.info?.(`Dashboard shop item ${row.id} created in ${guildId} by ${ctx.userId}`);
      return toItem(row);
    } catch (err) {
      throw mapError(err, name);
    }
  },

  async update(guildId, id, data) {
    const current = await prisma.shopItem.findFirst({ where: { id, guildId } });
    if (!current) throw new RecordError('not_found', 'That item no longer exists.');
    // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
    const problem = collection.check(data, { mode: 'update', current: toItem(current) });
    if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });
    const patch = { ...data };
    if (patch.name !== undefined) patch.name = checkName(patch.name);
    // Changing away from Role drops the role, as /economy-admin item edit does.
    if (patch.type !== undefined && patch.type !== 'ROLE' && current.type === 'ROLE') patch.roleId = null;
    if (Object.keys(patch).length === 0) return toItem(current);
    try {
      return toItem(await updateItem(guildId, id, patch));
    } catch (err) {
      throw mapError(err, patch.name ?? current.name);
    }
  },

  async remove(guildId, id, ctx = {}) {
    const row = await prisma.shopItem.findFirst({ where: { id, guildId }, select: { id: true } });
    if (!row) throw new RecordError('not_found', 'That item no longer exists.');
    const { copies } = await deleteItem(guildId, id);
    ctx.logger?.info?.(`Dashboard shop item ${id} deleted (${copies} copies) in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
