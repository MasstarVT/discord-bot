// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `automations` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'automations',
  module: 'automations',
  label: 'Automations',
  singular: 'automation',
  description: '',
  limit: LIMITS.AUTOMATIONS,
  titleField: 'id',
  canCreate: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
