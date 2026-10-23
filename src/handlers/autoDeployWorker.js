import { Worker, isMainThread, workerData } from 'node:worker_threads';
import { autoDeployCommands } from './commandLoader.js';
import logger from '../utils/logger.js';

// autoDeployCommands() imports every command file to hash the payloads. Run in the main thread, that graph (every
// command, the services behind it, a Redis client and a PrismaClient) stays loaded beside the ShardingManager for the
// life of the process, although the shard holds its own copy (measured: main heap 35 -> 17 MB, about 30 MB less RSS
// with a shard running). A short-lived worker thread runs it instead and takes all of that with it when it exits.

/** How long startup waits for the deploy. Past this the worker is stopped and the bot starts with the commands it has. */
export const AUTO_DEPLOY_TIMEOUT_MS = 120_000;

const ROLE_KEY = 'autoDeployWorker';

/**
 * Runs autoDeployCommands() in a worker thread and waits for it. Never rejects: a failed or stuck deploy is logged
 * and the bot starts with the commands Discord already has (as autoDeployCommands() does for a failed PUT).
 * @param {{ timeoutMs?: number, entry?: string|URL }} [opts] entry: the worker script (tests pass their own)
 * @returns {Promise<'done'|'failed'|'timeout'>}
 */
export function autoDeployInWorker({ timeoutMs = AUTO_DEPLOY_TIMEOUT_MS, entry = new URL(import.meta.url) } = {}) {
  return new Promise((resolve) => {
    let worker;
    try {
      worker = new Worker(entry, { workerData: { [ROLE_KEY]: true } });
    } catch (err) {
      logger.error('Could not start the auto-deploy worker — bot will still start', err);
      resolve('failed');
      return;
    }

    let timedOut = false;
    let timer = null;
    const finish = (outcome) => {
      clearTimeout(timer);
      resolve(outcome);
    };
    timer = setTimeout(() => {
      timedOut = true;
      logger.error(`Auto-deploy did not finish within ${timeoutMs / 1000} s — stopping it; bot will still start`);
      worker.terminate().then(() => finish('timeout'), () => finish('timeout'));
    }, timeoutMs);

    // 'exit' always follows 'error', and a second resolve is a no-op.
    worker.once('error', (err) => {
      logger.error('Auto-deploy worker crashed — bot will still start', err);
      finish('failed');
    });
    worker.once('exit', (code) => finish(timedOut ? 'timeout' : code === 0 ? 'done' : 'failed'));
  });
}

// ── Worker entry ──────────────────────────────────────────────────────────────
if (!isMainThread && workerData?.[ROLE_KEY] === true) {
  let code = 0;
  try {
    await autoDeployCommands();
  } catch (err) {
    logger.error('Auto-deploy failed — bot will still start', err);
    code = 1;
  }
  // The command graph opened a Redis client that would keep this thread alive. In a worker, process.exit ends only
  // this thread, and its log lines still reach the main thread's stdout.
  process.exit(code);
}
