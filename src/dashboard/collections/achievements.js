// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `achievements` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'achievements',
  module: 'achievements',
  label: 'Achievements',
  singular: 'achievement',
  description: '',
  limit: LIMITS.ACHIEVEMENTS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
