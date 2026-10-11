// The dashboard's database access for per-server records (saved embeds, custom commands, level rewards, ...):
// dispatches each request to the collection's store (`src/dashboard/collections/<key>.store.js`).
// Importing this file opens the Redis connection (the stores use the bot's services), so dashboard.js loads it with
// a dynamic import() only once the configuration is valid; tests inject fakes or import single stores.
import { RecordError } from './collections/errors.js';
import { createTaskRunner } from './tasks.js';
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
import { store as cases } from './collections/cases.store.js';
import { store as levels } from './collections/levels.store.js';
import { store as balances } from './collections/balances.store.js';
import { store as giveaways } from './collections/giveaways.store.js';
import { store as polls } from './collections/polls.store.js';
import { store as reactionRoles } from './collections/reactionRoles.store.js';
import { store as ticketPanels } from './collections/ticketPanels.store.js';
import { store as automations } from './collections/automations.store.js';

/** key -> store. Every key of COLLECTIONS (src/dashboard/collections/index.js) has one. */
export const STORES = Object.freeze({
  embeds, tags, triggers, reminders, feeds, levelRoles, shopItems, achievements, starboards, translateChannels,
  tempVoiceHubs, statChannels, economyBoosts, cases, levels, balances, giveaways, polls, reactionRoles, ticketPanels,
  automations,
});

const storeFor = (key) => {
  const store = Object.hasOwn(STORES, key) ? STORES[key] : null;
  if (!store) throw new RecordError('not_found', 'Unknown list.');
  return store;
};

/**
 * Creates the records bridge used by the dashboard server (`deps.records`). Every method may throw a RecordError
 * (expected failure, shown to the admin) or any other error (logged, answered as a 500).
 * Stores get a context `{ guildId, userId, user, member, discord, logger, runTask }`: the acting dashboard user (`user` =
 * `{ id, username, globalName, avatar }` from the session, for placeholders such as {user.name}; `member` =
 * `{ owner, permissions }` from that user's OAuth guild entry, for rules such as who may ping a role), the Discord
 * REST client (src/dashboard/discordApi.js) for writes such as posting a message, the logger, and
 * `runTask(kind, payload, opts?)` (src/dashboard/tasks.js), which has the bot do it for this user in this guild.
 * `list(guildId, opts)` gets `{ query, cursor }` for searchable collections and may return `{ items, nextCursor }`.
 * @param {{ discord: object, logger: object, tasks?: { run: Function } }} deps tasks: a test double of the task runner
 */
export function createRecordsBridge({ discord, logger, tasks = createTaskRunner({ logger }) }) {
  /** @param {{ userId: string, user?: object|null, member?: object|null }} actor */
  const ctx = (guildId, { userId, user = null, member = null }) => ({
    guildId, userId, user, member, discord, logger,
    runTask: (kind, payload, opts = {}) => tasks.run(guildId, kind, payload, { ...opts, actorId: userId }),
  });
  return {
    list: (guildId, key, opts = {}) => storeFor(key).list(guildId, opts),
    get: (guildId, key, id) => storeFor(key).get(guildId, id),
    create: (guildId, key, data, actor) => storeFor(key).create(guildId, data, ctx(guildId, actor)),
    update: (guildId, key, id, data, actor) => storeFor(key).update(guildId, id, data, ctx(guildId, actor)),
    remove: (guildId, key, id, actor) => storeFor(key).remove(guildId, id, ctx(guildId, actor)),
    async action(guildId, key, id, actionId, data, actor) {
      const store = storeFor(key);
      const run = Object.hasOwn(store.actions ?? {}, actionId) ? store.actions[actionId] : null;
      if (typeof run !== 'function') throw new RecordError('not_found', 'Unknown action.');
      return run(guildId, id, data, ctx(guildId, actor));
    },
  };
}
