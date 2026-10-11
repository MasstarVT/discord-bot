// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `giveaways` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'giveaways',
  module: 'giveaways',
  label: 'Giveaways',
  singular: 'giveaway',
  description: '',
  limit: LIMITS.ACTIVE_GIVEAWAYS,
  titleField: 'id',
  canCreate: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
