import 'dotenv/config';
import { ShardingManager } from 'discord.js';
import { fileURLToPath }   from 'url';
import { dirname, resolve } from 'path';
import logger               from './src/utils/logger.js';
import { deployCommands, autoDeployCommands } from './src/handlers/commandLoader.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

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

// ── Validate required env vars ────────────────────────────────────────────────
const required = ['DISCORD_TOKEN', 'CLIENT_ID', 'DATABASE_URL', 'REDIS_URL'];
const missing  = required.filter((k) => !process.env[k]);
if (missing.length > 0) {
  logger.error(`Missing required environment variables: ${missing.join(', ')}`);
  logger.error('Copy .env.example to .env and fill in all values.');
  process.exit(1);
}

// ── ShardingManager ───────────────────────────────────────────────────────────
// totalShards: 'auto' asks Discord for the recommended shard count.
// At < 2500 guilds this will always be 1 — no code change needed when scaling.
const manager = new ShardingManager(resolve(__dirname, 'src/bot.js'), {
  totalShards: 'auto',
  mode:        'worker',
  token:       process.env.DISCORD_TOKEN,
});

manager.on('shardCreate', (shard) => {
  logger.info(`Spawning Shard ${shard.id}...`);
  shard.on('ready',      () => logger.success(`Shard ${shard.id} is ready.`));
  shard.on('disconnect', () => logger.warn(`Shard ${shard.id} disconnected.`));
  shard.on('reconnecting', () => logger.info(`Shard ${shard.id} reconnecting...`));
  shard.on('death',      () => logger.error(`Shard ${shard.id} died.`));
  shard.on('error',  (err) => logger.error(`Shard ${shard.id} error`, err));
});

// Auto-deploy slash commands whenever the command set has changed
await autoDeployCommands();

logger.info('Starting ShardingManager...');
await manager.spawn();
