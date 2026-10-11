// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `starboards` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'starboards',
  module: 'starboard',
  label: 'Starboards',
  singular: 'starboard',
  description: '',
  limit: LIMITS.STARBOARDS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
