// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `tags` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'tags',
  module: 'customcommands',
  label: 'Custom commands',
  singular: 'custom command',
  description: '',
  limit: LIMITS.CUSTOM_TAGS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
