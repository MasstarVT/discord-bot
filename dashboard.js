// Web dashboard entry point: `npm run dashboard` (a separate process from the bot, sharing its database
// and Redis). Missing configuration parks the process instead of crash-looping: /healthz stays 200 and
// every other path answers 503 naming the keys to set.
import 'dotenv/config';
import logger from './src/utils/logger.js';
import { loadDashboardConfig } from './src/dashboard/config.js';
import { createDashboardServer, createParkedServer } from './src/dashboard/server.js';
import { createDiscordApi } from './src/dashboard/discordApi.js';

const SHUTDOWN_TIMEOUT_MS = 10_000;

process.on('unhandledRejection', (reason) => {
  logger.error('Unhandled Promise Rejection', reason instanceof Error ? reason : new Error(String(reason)));
});

process.on('uncaughtException', (err) => {
  logger.error('Uncaught Exception', err);
  process.exit(1);
});

const { ok, missing, invalid, warnings, config } = loadDashboardConfig();
for (const warning of warnings) logger.warn(`Dashboard: ${warning}`);

let server;
let closeServices = async () => {};

if (!ok) {
  if (missing.length > 0) logger.error(`Dashboard not started: missing ${missing.join(', ')}.`);
  for (const { key, reason } of invalid) logger.error(`Dashboard not started: ${key} ${reason}.`);
  logger.error('Set these keys (see the "Web dashboard" section of .env.example) and restart. '
    + 'Parked: /healthz answers 200, every other path 503.');
  server = createParkedServer({ missing, invalid });
} else {
  // Loaded only now: importing the settings gateway and the record stores opens the Redis connection.
  const settings = await import('./src/dashboard/settingsBridge.js');
  const { createRecordsBridge } = await import('./src/dashboard/recordsBridge.js');
  const botJoins = await import('./src/services/botJoinService.js');
  const discord = createDiscordApi({ config, logger });
  const records = createRecordsBridge({ discord, logger });
  server = createDashboardServer({ config, deps: { discord, settings, records, botJoins }, logger });
  if (config.urlIsDefault && config.host === '0.0.0.0') {
    logger.warn(`Dashboard: DASHBOARD_URL is not set, so saving only works at ${config.url}. Browsers that open the dashboard `
      + 'by another address will be able to read but not save changes until DASHBOARD_URL is set to that address.');
  }
  closeServices = async () => {
    const { disconnect } = await import('./src/database/client.js');
    const { default: redis } = await import('./src/services/redis.js');
    await Promise.allSettled([disconnect(), redis.quit()]);
  };
}

server.on('error', (err) => {
  logger.error(`Dashboard server on ${config.host}:${config.port} failed`, err);
  process.exit(1);
});

server.listen(config.port, config.host, () => {
  if (ok) logger.success(`Dashboard listening on ${config.host}:${config.port} (public URL ${config.url}).`);
  else logger.warn(`Dashboard parked on ${config.host}:${config.port}.`);
});

let shuttingDown = false;
async function shutdown(signal) {
  if (shuttingDown) {
    logger.warn(`${signal} received again — exiting now.`);
    process.exit(1);
  }
  shuttingDown = true;
  logger.info(`${signal} received — shutting down the dashboard...`);
  setTimeout(() => {
    logger.error(`Dashboard shutdown took longer than ${SHUTDOWN_TIMEOUT_MS / 1000} s — forcing exit.`);
    process.exit(1);
  }, SHUTDOWN_TIMEOUT_MS).unref();

  server.close();
  server.closeAllConnections();
  try {
    await closeServices();
  } catch (err) {
    logger.warn(`Dashboard shutdown: ${err?.message ?? err}`);
  }
  process.exit(0);
}

process.on('SIGTERM', () => void shutdown('SIGTERM'));
process.on('SIGINT', () => void shutdown('SIGINT'));
