// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `polls` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'polls',
  module: 'polls',
  label: 'Polls',
  singular: 'poll',
  description: '',
  limit: LIMITS.ACTIVE_POLLS,
  titleField: 'id',
  canCreate: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
