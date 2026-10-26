import { Client, Collection, Events, GatewayIntentBits, Options, Partials } from 'discord.js';
import { loadCommands } from './handlers/commandLoader.js';
import { loadEvents }   from './handlers/eventLoader.js';
import { createHandlerMaps, registerCoreComponents } from './handlers/componentRegistry.js';
import { collectPlugins, installPlugins } from './handlers/pluginLoader.js';
import { disconnect as disconnectDatabase } from './database/client.js';
import redis             from './services/redis.js';
import { flushLogQueue } from './services/loggingService.js';
import { flushMessageCounts } from './services/achievementService.js';
import { stop as stopScheduler } from './services/scheduler.js';
import { withTimeout } from './utils/async.js';
import { FATAL_CLOSE_CODES, FATAL_MESSAGE_KEY, fatalCodeFromError } from './utils/fatalGateway.js';
import { presencesEnabled } from './utils/gatewayFeatures.js';
import logger            from './utils/logger.js';

/** Cached messages older than this are swept. Logging then has no old content for deleting or editing them. */
const MESSAGE_CACHE_LIFETIME_S = 3 * 60 * 60;
const MESSAGE_SWEEP_INTERVAL_S = 30 * 60;

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMembers,          // Privileged
    GatewayIntentBits.GuildModeration,
    GatewayIntentBits.GuildEmojisAndStickers,
    GatewayIntentBits.GuildIntegrations,
    GatewayIntentBits.GuildWebhooks,
    GatewayIntentBits.GuildInvites,
    GatewayIntentBits.GuildVoiceStates,
    GatewayIntentBits.GuildScheduledEvents,  // events: a Discord event cancelled in the Events tab cancels its /event
    // Privileged; used by stat-channels (ONLINE) and liverole (R208). DISABLE_PRESENCES=1 drops it (the busiest event).
    ...(presencesEnabled() ? [GatewayIntentBits.GuildPresences] : []),
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.GuildMessageReactions,
    GatewayIntentBits.DirectMessages,
    GatewayIntentBits.MessageContent,        // Privileged
  ],
  // Caches nothing reads are kept empty. Members and presences stay unlimited: stat channels, the live role and role
  // checks count or read them. Stickers are counted from the REST guild in /info server.
  makeCache: Options.cacheWithLimits({
    ...Options.DefaultMakeCacheSettings, // MessageManager: 200 per channel
    GuildStickerManager: 0,
    GuildScheduledEventManager: 0,
    StageInstanceManager: 0,
    ThreadMemberManager: { maxSize: 0, keepOverLimit: (m) => m.id === m.client.user?.id }, // keeps thread.joined right
  }),
  sweepers: {
    ...Options.DefaultSweeperSettings, // archived threads
    messages: { interval: MESSAGE_SWEEP_INTERVAL_S, lifetime: MESSAGE_CACHE_LIFETIME_S },
  },
  partials: [
    Partials.Channel,
    Partials.Message,
    Partials.Reaction,
    Partials.GuildMember,
    Partials.User,
    Partials.GuildScheduledEvent, // scheduled events are not cached: without it a delete is never emitted
  ],
  // Bot replies and logs can quote user text: never let it ping roles or @everyone (admin-configured
  // messages that must ping a role opt in per message), and don't ping the author on replies.
  allowedMentions: { parse: ['users'], repliedUser: false },
});

// ── Component handler registries ─────────────────────────────────────────────
// Plugins and commands register their handlers here at startup. Keys follow the customId
// namespace: "prefix" from "prefix:param1:param2". The maps refuse a duplicate prefix.
client.commands           = new Collection();
client.commandCategories  = new Map();
createHandlerMaps(client); // client.buttonHandlers / selectHandlers / modalHandlers

// ── Fatal gateway close codes ────────────────────────────────────────────────
// 4004 / 4013 / 4014 (bad token, invalid or disallowed intents) never fix
// themselves. Tell the ShardingManager so it parks instead of restarting.
// prependListener: this must reach the manager before discord.js's own
// shardDisconnect → "_disconnect" message does.
let fatalReported = false;

function reportFatal(code) {
  if (fatalReported || !FATAL_CLOSE_CODES[code]) return;
  fatalReported = true;
  client.shard?.send({ [FATAL_MESSAGE_KEY]: code }).catch(() => {});
}

client.prependListener(Events.ShardDisconnect, (event) => reportFatal(event?.code));

// ── Non-fatal errors ─────────────────────────────────────────────────────────
// A dying shard ends the whole process (index.js), so one failed event or
// collector callback must not kill it. The client captures rejections from
// its async listeners and re-emits them as "error", which would throw with
// no listener. Other stray rejections (e.g. collector callbacks) land in the
// worker's own unhandledRejection handler; the main thread's doesn't see them.
client.on(Events.Error, (err) => logger.error('Discord client error', err));

process.on('unhandledRejection', (reason) => {
  logger.error('Unhandled Promise Rejection in shard', reason instanceof Error ? reason : new Error(String(reason)));
});

const LOG_FLUSH_TIMEOUT_MS = 4000;
const ACHIEVEMENT_FLUSH_TIMEOUT_MS = 3000;

// ── Shutdown hook ────────────────────────────────────────────────────────────
// Worker threads don't receive signals. index.js calls this on each shard via
// eval when it gets SIGTERM/SIGINT.
client.shutdown = async () => {
  // Stop claiming jobs and let the running ones finish (bounded) before the client goes away.
  try {
    await stopScheduler({ timeoutMs: 5000 });
  } catch (err) {
    logger.error('Scheduler did not stop cleanly', err);
  }
  // Achievement message counts wait in memory for a few seconds: write them while the database and the client are still
  // up (an unlock pays role rewards), and write any later message at once.
  try {
    await withTimeout(flushMessageCounts({ final: true }), ACHIEVEMENT_FLUSH_TIMEOUT_MS, 'achievement count flush');
  } catch (err) {
    logger.warn(`Achievement message counts were not written before shutdown: ${err?.message ?? err}`);
  }
  // Audit-log embeds are coalesced in a per-channel queue (about 1 s): post what is waiting while the client can
  // still send. Bounded, so a stuck channel cannot hold the shutdown (index.js force-exits after 25 s).
  try {
    await withTimeout(flushLogQueue(), LOG_FLUSH_TIMEOUT_MS, 'log queue flush');
  } catch (err) {
    logger.warn(`Log queue was not flushed before shutdown: ${err?.message ?? err}`);
  }
  await client.destroy();
  await Promise.allSettled([
    disconnectDatabase(),
    redis.status === 'ready' ? redis.quit() : redis.disconnect(),
  ]);
};

// ── Boot sequence ─────────────────────────────────────────────────────────────
async function start() {
  // Plugins first: their hooks, persistent components, job handlers and reward providers must exist
  // before any event can fire. registerCoreComponents then adds the core's own handlers.
  const collected = await collectPlugins();
  installPlugins(client, collected);
  registerCoreComponents(client);

  logger.info('Loading commands...');
  await loadCommands(client);

  logger.info('Loading events...');
  await loadEvents(client);

  logger.info('Logging in to Discord...');
  await client.login(process.env.DISCORD_TOKEN);
}

start().catch((err) => {
  const code = fatalCodeFromError(err);
  if (code) {
    // Report before exiting so the manager parks rather than restarting.
    reportFatal(code);
    logger.error(`Discord login failed with ${code} (${FATAL_CLOSE_CODES[code].name}). ${FATAL_CLOSE_CODES[code].fix}`);
  } else {
    logger.error('Fatal error during bot startup', err);
  }
  process.exit(1); // ends this worker thread; index.js decides what happens next
});

export default client;
