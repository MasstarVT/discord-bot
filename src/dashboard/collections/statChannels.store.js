// PLACEHOLDER (replaced by the collection's implementation): database access for the `statChannels` collection.
import { RecordError } from './errors.js';

const unavailable = async () => {
  throw new RecordError('unavailable', 'This list cannot be managed from the dashboard yet.');
};

export const store = Object.freeze({
  list: async () => [],
  get: async () => null,
  create: unavailable,
  update: unavailable,
  remove: unavailable,
  actions: Object.freeze({}),
});
