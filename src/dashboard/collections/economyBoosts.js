// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `economyBoosts` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'economyBoosts',
  module: 'economy',
  label: 'Coin boosts',
  singular: 'coin boost',
  description: '',
  limit: LIMITS.ECONOMY_BOOSTS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
