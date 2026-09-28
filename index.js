import 'dotenv/config';
import { ShardingManager } from 'discord.js';
import { fileURLToPath }   from 'url';
import { dirname, resolve } from 'path';
import { setTimeout as sleep } from 'timers/promises';
import logger               from './src/utils/logger.js';
import { deployCommands, autoDeployCommands } from './src/handlers/commandLoader.js';
import { startHealthServer } from './src/utils/healthServer.js';
import { recordStart }       from './src/utils/restartBrake.js';
import { FATAL_CLOSE_CODES, FATAL_MESSAGE_KEY, fatalCodeFromError } from './src/utils/fatalGateway.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

const HEALTH_PORT            = Number(process.env.HEALTH_PORT || 8081);
const STATE_DIR              = resolve(__dirname, process.env.STATE_DIR || 'state');
const READY_CHECK_TIMEOUT_MS = 5_000;
const SPAWN_READY_TIMEOUT_MS = 60_000;
const SHUTDOWN_TIMEOUT_MS    = 25_000;      // Docker's stop_grace_period is 30 s
const PARK_REMINDER_MS       = 10 * 60_000;

// ── Top-level error guards ────────────────────────────────────────────────────
// Must be registered before any async work so crashes surface with full traces.
process.on('unhandledRejection', (reason) => {
  logger.error('Unhandled Promise Rejection', reason instanceof Error ? reason : new Error(String(reason)));
});

process.on('uncaughtException', (err) => {
  logger.error('Uncaught Exception', err);
  process.exit(1);
});

// ── Deploy-only mode ──────────────────────────────────────────────────────────
// Run `node index.js --deploy` (or `npm run deploy`) to push slash commands,
// then exit. Does not start the bot.
if (process.argv.includes('--deploy')) {
  logger.info('Deploy mode — registering slash commands...');
  await deployCommands();
  process.exit(0);
}

// ── Lifecycle state (read by /readyz) ─────────────────────────────────────────
let manager      = null;
let phase        = 'starting';   // 'starting' → ('brake-delay' →) 'spawned'
let brakeUntil   = 0;
let parked       = null;         // fatal close code or 'missing-env' once parked
let shuttingDown = false;

function withTimeout(promise, ms, what) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} timed out after ${ms} ms`)), ms);
    }),
  ]).finally(() => clearTimeout(timer));
}

async function getReadiness() {
  if (parked) return { ready: false, reason: `parked:${parked}` };
  if (phase === 'brake-delay') {
    return { ready: false, reason: 'brake-delay', until: new Date(brakeUntil).toISOString() };
  }
  if (!manager || phase !== 'spawned') return { ready: false, reason: 'starting' };

  const states = await withTimeout(
    manager.broadcastEval((c) => c.isReady()),
    READY_CHECK_TIMEOUT_MS,
    'Shard ready check',
  );
  return states.length > 0 && states.every(Boolean)
    ? { ready: true, shards: states.length }
    : { ready: false, reason: 'shard-not-ready', shards: states.length };
}

/**
 * Stops for good without exiting: no respawn, no restart by Docker (the
 * process stays up and /healthz stays 200), /readyz reports "parked:<code>".
 * Used when retrying cannot help and would only burn Discord logins.
 */
function park(code, message) {
  if (parked) return;
  parked = code;
  const text = message ?? `Discord closed the gateway with ${code} (${FATAL_CLOSE_CODES[code].name}). ${FATAL_CLOSE_CODES[code].fix}`;
  logger.error(`PARKED: ${text}`);
  logger.error(`Not retrying. /healthz stays 200 and /readyz returns 503 "parked:${code}" until the bot is restarted.`);
  // Repeat the reason in the logs; the interval also keeps the process alive.
  setInterval(() => logger.error(`Still parked (${code}): ${text}`), PARK_REMINDER_MS);
}

// ── Health endpoints ──────────────────────────────────────────────────────────
// Started before anything slow so Docker's HEALTHCHECK passes during the
// restart brake and while parked.
const healthServer = startHealthServer({
  port: HEALTH_PORT,
  getReadiness,
  onError: (err) => logger.error(`Health server on port ${HEALTH_PORT} failed — /healthz and /readyz are unavailable`, err),
});
healthServer.once('listening', () => logger.info(`Health endpoints listening on :${HEALTH_PORT} (/healthz, /readyz)`));

// ── Graceful shutdown ─────────────────────────────────────────────────────────
// The only signal handler in the app. Worker threads get no signals, so each
// shard is asked to disconnect from Discord (client.shutdown in src/bot.js,
// falling back to client.destroy) before the process exits.
async function shutdown(signal) {
  if (shuttingDown) {
    logger.warn(`${signal} received again — exiting now.`);
    process.exit(1);
  }
  shuttingDown = true;
  logger.info(`${signal} received — shutting down...`);
  setTimeout(() => {
    logger.error(`Shutdown took longer than ${SHUTDOWN_TIMEOUT_MS / 1000} s — forcing exit.`);
    process.exit(1);
  }, SHUTDOWN_TIMEOUT_MS).unref();

  healthServer.close();
  healthServer.closeAllConnections();

  // Same as manager.broadcastEval, but per shard so a dead or still-spawning
  // shard doesn't stop the others from disconnecting cleanly.
  const live = manager ? [...manager.shards.values()].filter((s) => s.worker ?? s.process) : [];
  if (live.length > 0) {
    const results = await withTimeout(
      Promise.allSettled(live.map((s) => s.eval((c) => (typeof c.shutdown === 'function' ? c.shutdown() : c.destroy())))),
      SHUTDOWN_TIMEOUT_MS - 5_000,
      'Shard shutdown',
    ).catch((err) => [{ status: 'rejected', reason: err }]);
    const failed = results.filter((r) => r.status === 'rejected');
    if (failed.length > 0) logger.warn(`Shard shutdown incomplete: ${failed.map((r) => r.reason?.message).join('; ')}`);
    else logger.info(`Disconnected ${live.length} shard(s) from Discord.`);
  }
  process.exit(0);
}

process.on('SIGTERM', () => void shutdown('SIGTERM'));
process.on('SIGINT',  () => void shutdown('SIGINT'));

// ── Boot ──────────────────────────────────────────────────────────────────────
async function boot() {
  // Missing config can't fix itself, so park rather than crash-loop.
  const required = ['DISCORD_TOKEN', 'CLIENT_ID', 'DATABASE_URL', 'REDIS_URL'];
  const missing  = required.filter((k) => !process.env[k]);
  if (missing.length > 0) {
    park('missing-env', `Missing required environment variables: ${missing.join(', ')}. ` +
      'Copy .env.example to .env (or set them on the container) and restart.');
    return;
  }

  // Restart brake: pace crash loops so they can't use up the daily login limit.
  const brake = recordStart(STATE_DIR);
  if (brake.warning) logger.warn(`Restart brake: ${brake.warning}`);
  if (brake.delayMs > 0) {
    phase      = 'brake-delay';
    brakeUntil = Date.now() + brake.delayMs;
    logger.warn(`Restart brake: ${brake.starts} starts in the last hour — waiting ` +
      `${brake.delayMs / 60_000} min before connecting to Discord (until ${new Date(brakeUntil).toISOString()}).`);
    await sleep(brake.delayMs);
    phase = 'starting';
  } else {
    logger.info(`Restart brake: start ${brake.starts} in the last hour (more than 5 are delayed).`);
  }

  // Auto-deploy slash commands whenever the command set has changed
  await autoDeployCommands();

  // ── ShardingManager ─────────────────────────────────────────────────────────
  // totalShards: 'auto' asks Discord for the recommended shard count.
  // At < 2500 guilds this will always be 1 — no code change needed when scaling.
  // respawn: false — a dead shard ends the process, so Docker's restart policy
  // and the restart brake above decide when to try again.
  manager = new ShardingManager(resolve(__dirname, 'src/bot.js'), {
    totalShards: 'auto',
    mode:        'worker',
    respawn:     false,
    token:       process.env.DISCORD_TOKEN,
  });

  manager.on('shardCreate', (shard) => {
    logger.info(`Spawning Shard ${shard.id}...`);
    shard.on('ready',      () => logger.success(`Shard ${shard.id} is ready.`));
    shard.on('reconnecting', () => logger.info(`Shard ${shard.id} reconnecting...`));
    shard.on('error',  (err) => logger.error(`Shard ${shard.id} error`, err));
    // Fatal close codes arrive here first (see reportFatal in src/bot.js).
    shard.on('message', (message) => {
      const code = message?.[FATAL_MESSAGE_KEY];
      if (FATAL_CLOSE_CODES[code]) park(code);
    });
    // discord.js only reports "disconnect" for close codes it will not
    // reconnect from. The non-fatal ones (e.g. 4011 sharding required) can be
    // fixed by a fresh start, so exit and let Docker restart us.
    shard.on('disconnect', () => {
      if (parked || shuttingDown) {
        logger.warn(`Shard ${shard.id} disconnected.`);
        return;
      }
      logger.error(`Shard ${shard.id} disconnected and won't reconnect — exiting so Docker restarts the bot (the restart brake paces retries).`);
      process.exit(1);
    });
    shard.on('death', () => {
      if (shuttingDown) return;
      if (parked) {
        logger.warn(`Shard ${shard.id} exited (parked: ${parked}).`);
        return;
      }
      logger.error(`Shard ${shard.id} died — exiting so Docker restarts the bot (the restart brake paces retries).`);
      process.exit(1);
    });
  });

  logger.info('Starting ShardingManager...');
  try {
    await manager.spawn({ timeout: SPAWN_READY_TIMEOUT_MS });
  } catch (err) {
    if (parked || shuttingDown) return;

    const code = fatalCodeFromError(err);
    if (code) {
      park(code);
      return;
    }

    // Every shard is running but slow to get ready (e.g. a Discord incident):
    // keep going and let /readyz report it instead of forcing another login.
    if (err?.code === 'ShardingReadyTimeout' && manager.shards.size === manager.totalShards) {
      logger.warn(`Shards not ready after ${SPAWN_READY_TIMEOUT_MS / 1000} s — still waiting; /readyz returns 503 until they are.`);
    } else {
      // fetchRecommendedShardCount throws the raw fetch Response on HTTP errors.
      const error = err instanceof Error ? err : new Error(`Discord API returned HTTP ${err?.status ?? 'error'}`);
      logger.error('Failed to spawn shards — exiting so Docker restarts the bot (the restart brake paces retries).', error);
      process.exit(1);
    }
  }
  phase = 'spawned';
}

await boot().catch((err) => {
  logger.error('Fatal error during startup', err);
  process.exit(1);
});
