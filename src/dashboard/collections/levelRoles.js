// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `levelRoles` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'levelRoles',
  module: 'leveling',
  label: 'Level rewards',
  singular: 'level reward',
  description: '',
  limit: LIMITS.LEVEL_ROLES,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
