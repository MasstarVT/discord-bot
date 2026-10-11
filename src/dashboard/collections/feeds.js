// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `feeds` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'feeds',
  module: 'feeds',
  label: 'Social feeds',
  singular: 'feed',
  description: '',
  limit: LIMITS.FEEDS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
