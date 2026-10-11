// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `embeds` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'embeds',
  module: 'embeds',
  label: 'Saved embeds',
  singular: 'saved embed',
  description: '',
  limit: LIMITS.SAVED_EMBEDS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
