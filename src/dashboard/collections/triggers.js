// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `triggers` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'triggers',
  module: 'customcommands',
  label: 'Auto-responders',
  singular: 'auto-responder',
  description: '',
  limit: LIMITS.AUTO_TRIGGERS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
