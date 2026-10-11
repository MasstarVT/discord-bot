// Descriptor of the `shopItems` collection: the items members buy with /shop buy (managed with /economy-admin item ...).
// Pure: constants only. The rules mirror itemCreate and itemEdit in src/commands/admin/economy-admin.js; the limits that live
// in economyService.js are copied here (the service opens Redis and Prisma at import) and test/dashboard-collections-more.test.js
// fails when they drift apart.
import { COIN_MAX, LIMITS } from '../../config/constants.js';

export const ITEM_NAME_MAX = 50;
export const ITEM_DESCRIPTION_MAX = 200;
export const ITEM_MESSAGE_MAX = 1000;
export const ITEM_IMAGE_MAX = 500;
export const MAX_STOCK = 1_000_000;

/** Variables the buy and use messages understand (itemVars in economyService.js). */
export const ITEM_MESSAGE_TOKENS = Object.freeze(['{item}', '{quantity}', '{coins}', '{price}', '{currency}', '{currency.emoji}']);

const bool = (field, label, description, def) => Object.freeze({ field, type: 'boolean', label, description, default: def });

const fields = Object.freeze([
  Object.freeze({
    field: 'name',
    type: 'text',
    label: 'Name',
    description: 'Shown in /shop and used to pick the item. Unique on this server (upper and lower case count as the same).',
    maxLength: ITEM_NAME_MAX,
    minLength: 1,
    default: null,
    required: true,
  }),
  Object.freeze({
    field: 'price',
    type: 'int',
    label: 'Price',
    description: 'What one copy costs, in the server currency.',
    min: 1,
    max: COIN_MAX,
    default: 100,
    required: true,
  }),
  Object.freeze({
    field: 'type',
    type: 'enum',
    label: 'Type',
    description: 'Collectible items just sit in the inventory. Usable items are used with /use. Role items give a role.',
    options: Object.freeze([
      Object.freeze({ value: 'STATIC', label: 'Collectible' }),
      Object.freeze({ value: 'USABLE', label: 'Usable (with /use)' }),
      Object.freeze({ value: 'ROLE', label: 'Role (gives a role)' }),
    ]),
    default: 'STATIC',
  }),
  Object.freeze({
    field: 'roleId',
    type: 'role',
    label: 'Role',
    description: 'The role a Role item gives. Only for Role items; it must be below the bot\'s highest role.',
    assignable: true,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'description',
    type: 'text',
    label: 'Description',
    description: 'Shown in the shop.',
    multiline: true,
    maxLength: ITEM_DESCRIPTION_MAX,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'imageUrl',
    type: 'url',
    label: 'Image link',
    description: 'A picture shown with the item (an http or https link).',
    maxLength: ITEM_IMAGE_MAX,
    nullable: true,
    default: null,
  }),
  bool('useOnBuy', 'Use on purchase', 'The item is used the moment it is bought. Not for collectible items.', false),
  bool('destroyOnUse', 'Used up on use', 'Each use removes one copy from the inventory.', true),
  bool('canBuyMultiple', 'Allow several copies', 'Members may own more than one copy (up to the limit below). Off: one per member.', true),
  Object.freeze({
    field: 'maxPerUser',
    type: 'int',
    label: 'Copies per member',
    description: 'The most copies one member may own.',
    min: 1,
    max: LIMITS.ITEM_COPIES_PER_USER,
    default: LIMITS.ITEM_COPIES_PER_USER,
  }),
  Object.freeze({
    field: 'stock',
    type: 'int',
    label: 'Stock',
    description: 'Copies left for sale. Empty means unlimited; 0 means sold out.',
    min: 0,
    max: MAX_STOCK,
    nullable: true,
    default: null,
  }),
  Object.freeze({
    field: 'buyMessage',
    type: 'text',
    label: 'Purchase message',
    description: 'Shown after a purchase. Empty uses the default message.',
    multiline: true,
    maxLength: ITEM_MESSAGE_MAX,
    nullable: true,
    default: null,
    placeholders: ITEM_MESSAGE_TOKENS,
  }),
  Object.freeze({
    field: 'useMessage',
    type: 'text',
    label: 'Use message',
    description: 'Shown after the item is used. Empty uses the default message.',
    multiline: true,
    maxLength: ITEM_MESSAGE_MAX,
    nullable: true,
    default: null,
    placeholders: ITEM_MESSAGE_TOKENS,
  }),
  bool('enabled', 'In the shop', 'Off hides the item from /shop and stops purchases. Copies members own are kept.', true),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'summary', type: 'text', label: 'Summary', maxLength: 100, default: '' }),
  Object.freeze({ field: 'createdAt', type: 'datetime', label: 'Added', nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'shopItems',
  module: 'economy',
  label: 'Shop items',
  singular: 'shop item',
  description: 'What members can buy with /shop buy. Deleting an item also deletes every copy members own. '
    + 'Role items need a role the bot can give (it must be below the bot\'s highest role).',
  limit: LIMITS.SHOP_ITEMS,
  titleField: 'name',
  subtitleField: 'summary',
  fields,
  createOnly: Object.freeze([]),
  readOnly,
  actions: Object.freeze([]),
  /**
   * Cross-field rules of /economy-admin item create and edit. On an update the merged item is checked:
   * `current` is the stored item and `data` the changed fields.
   */
  check(data, { mode, current } = {}) {
    const merged = { ...(mode === 'update' ? current : {}), ...data };
    const name = (data.name ?? '').trim();
    if (data.name !== undefined && name.toLowerCase() === 'none') {
      return { path: 'name', message: 'Pick a different name.' };
    }
    if (merged.type === 'ROLE' && !merged.roleId) {
      return { path: 'roleId', message: 'A Role item needs a role.' };
    }
    if (merged.type !== 'ROLE' && data.roleId) {
      return { path: 'roleId', message: 'Only Role items can have a role. Set the type to Role as well.' };
    }
    if (merged.type === 'STATIC' && merged.useOnBuy === true) {
      return { path: 'useOnBuy', message: "Collectible items can't be used, so they can't be used on purchase." };
    }
    // /economy-admin item create asks for a stock of at least 1 (empty = unlimited); only edits may set 0 = sold out.
    if (mode === 'create' && data.stock === 0) {
      return { path: 'stock', message: 'Start with a stock of at least 1, or leave it empty for unlimited. You can set 0 later.' };
    }
    return null;
  },
});
