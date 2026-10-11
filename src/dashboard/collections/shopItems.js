// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `shopItems` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'shopItems',
  module: 'economy',
  label: 'Shop items',
  singular: 'shop item',
  description: '',
  limit: LIMITS.SHOP_ITEMS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
