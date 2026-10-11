// The dashboard's database access for per-server records (saved embeds, custom commands, level rewards, ...):
// dispatches each request to the collection's store (`src/dashboard/collections/<key>.store.js`).
// Importing this file opens the Redis connection (the stores use the bot's services), so dashboard.js loads it with
// a dynamic import() only once the configuration is valid; tests inject fakes or import single stores.
import { RecordError } from './collections/errors.js';
import { store as embeds } from './collections/embeds.store.js';
import { store as tags } from './collections/tags.store.js';
import { store as triggers } from './collections/triggers.store.js';
import { store as reminders } from './collections/reminders.store.js';
import { store as feeds } from './collections/feeds.store.js';
import { store as levelRoles } from './collections/levelRoles.store.js';
import { store as shopItems } from './collections/shopItems.store.js';
import { store as achievements } from './collections/achievements.store.js';
import { store as starboards } from './collections/starboards.store.js';
import { store as translateChannels } from './collections/translateChannels.store.js';
import { store as tempVoiceHubs } from './collections/tempVoiceHubs.store.js';
import { store as statChannels } from './collections/statChannels.store.js';
import { store as economyBoosts } from './collections/economyBoosts.store.js';

/** key -> store. Every key of COLLECTIONS (src/dashboard/collections/index.js) has one. */
export const STORES = Object.freeze({
  embeds, tags, triggers, reminders, feeds, levelRoles, shopItems, achievements, starboards, translateChannels,
  tempVoiceHubs, statChannels, economyBoosts,
});

const storeFor = (key) => {
  const store = Object.hasOwn(STORES, key) ? STORES[key] : null;
  if (!store) throw new RecordError('not_found', 'Unknown list.');
  return store;
};

/**
 * Creates the records bridge used by the dashboard server (`deps.records`). Every method may throw a RecordError
 * (expected failure, shown to the admin) or any other error (logged, answered as a 500).
 * Stores get a context `{ userId, discord, logger }`: the acting dashboard user, the Discord REST client
 * (src/dashboard/discordApi.js) for writes such as posting a message, and the logger.
 * @param {{ discord: object, logger: object }} deps
 */
export function createRecordsBridge({ discord, logger }) {
  const ctx = (userId) => ({ userId, discord, logger });
  return {
    list: (guildId, key) => storeFor(key).list(guildId),
    get: (guildId, key, id) => storeFor(key).get(guildId, id),
    create: (guildId, key, data, { userId }) => storeFor(key).create(guildId, data, ctx(userId)),
    update: (guildId, key, id, data, { userId }) => storeFor(key).update(guildId, id, data, ctx(userId)),
    remove: (guildId, key, id, { userId }) => storeFor(key).remove(guildId, id, ctx(userId)),
    async action(guildId, key, id, actionId, data, { userId }) {
      const store = storeFor(key);
      const run = Object.hasOwn(store.actions ?? {}, actionId) ? store.actions[actionId] : null;
      if (typeof run !== 'function') throw new RecordError('not_found', 'Unknown action.');
      return run(guildId, id, data, ctx(userId));
    },
  };
}
