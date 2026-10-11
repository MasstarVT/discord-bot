// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `reactionRoles` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'reactionRoles',
  module: 'reactionroles',
  label: 'Reaction role panels',
  singular: 'panel',
  description: '',
  limit: LIMITS.REACTION_ROLE_PANELS,
  titleField: 'id',
  canCreate: false,
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
