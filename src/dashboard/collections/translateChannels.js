// PLACEHOLDER (replaced by the collection's implementation): descriptor of the `translateChannels` collection.
import { LIMITS } from '../../config/constants.js';

export const collection = Object.freeze({
  key: 'translateChannels',
  module: 'translation',
  label: 'Translated channels',
  singular: 'translated channel',
  description: '',
  limit: LIMITS.TRANSLATE_CHANNELS,
  titleField: 'id',
  fields: Object.freeze([]),
  createOnly: Object.freeze([]),
  readOnly: Object.freeze([]),
  actions: Object.freeze([]),
  placeholder: true,
});
