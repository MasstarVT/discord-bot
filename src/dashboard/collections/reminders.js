// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `reminders` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'reminders',
  module: 'reminders',
  label: 'Reminders',
  singular: 'reminder',
  description: '',
  limit: LIMITS.REMINDERS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
