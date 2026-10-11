// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `statChannels` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'statChannels',
  module: 'stats',
  label: 'Statistics channels',
  singular: 'statistics channel',
  description: '',
  limit: LIMITS.STAT_CHANNELS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
