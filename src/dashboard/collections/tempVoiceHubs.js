// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `tempVoiceHubs` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'tempVoiceHubs',
  module: 'tempvoice',
  label: 'Join-to-create hubs',
  singular: 'hub',
  description: '',
  limit: LIMITS.TEMPVOICE_HUBS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
