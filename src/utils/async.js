// Small async helpers shared by every package (01-foundation §7.2).
import logger from './logger.js';

// ── Timeouts ──────────────────────────────────────────────────────────────────

export class TimeoutError extends Error {
  constructor(label) {
    super(`${label} timed out`);
    this.name = 'TimeoutError';
    this.label = label;
  }
}

/** @param {number} ms @returns {Promise<void>} */
export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Resolves or rejects with `promise`, or rejects with TimeoutError(label) after `ms`.
 * When the timeout wins the original promise keeps running; a later rejection is logged
 * so it can never become an unhandled rejection.
 * @template T
 * @param {Promise<T>|T} promise
 * @param {number} ms
 * @param {string} [label]
 * @returns {Promise<T>}
 */
export function withTimeout(promise, ms, label = 'operation') {
  const p = Promise.resolve(promise);
  return new Promise((resolve, reject) => {
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      p.catch((err) => logger.error(`${label} failed after timeout`, err));
      reject(new TimeoutError(label));
    }, Math.max(0, ms));
    p.then(
      (value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(value);
      },
      (err) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        reject(err);
      },
    );
  });
}

/**
 * Time left for lookups before an interaction MUST be acknowledged. Discord allows 3 s;
 * 2600 ms leaves network slack. Use it as the `ms` of withTimeout for every lookup that
 * precedes deferReply / deferUpdate / update / showModal; at 0 skip optional lookups and
 * acknowledge immediately.
 * @param {{ createdTimestamp: number }} interaction
 * @param {number} [reserveMs=400]
 * @returns {number}
 */
export function ackBudgetMs(interaction, reserveMs = 400) {
  return Math.max(0, 2600 - (Date.now() - interaction.createdTimestamp) - reserveMs);
}

// ── Concurrency ───────────────────────────────────────────────────────────────

/**
 * Runs `fn` over `items` with at most `limit` in flight. Order preserved; never rejects.
 * @template T, R
 * @param {T[]} items
 * @param {number} limit
 * @param {(item: T, index: number) => Promise<R>|R} fn
 * @returns {Promise<Array<{ status: 'fulfilled', value: R } | { status: 'rejected', reason: any }>>}
 */
export async function mapLimit(items, limit, fn) {
  const list = Array.from(items ?? []);
  const results = new Array(list.length);
  let next = 0;
  const worker = async () => {
    while (next < list.length) {
      const index = next++;
      try {
        results[index] = { status: 'fulfilled', value: await fn(list[index], index) };
      } catch (reason) {
        results[index] = { status: 'rejected', reason };
      }
    }
  };
  const workers = Math.max(1, Math.min(Math.floor(limit) || 1, list.length));
  await Promise.all(Array.from({ length: workers }, worker));
  return results;
}

/**
 * Serialises work per key with an in-process promise chain; the entry is deleted when idle.
 * `run` links `fn` into the per-key chain synchronously, before it returns (callers rely on
 * gateway order), and a rejecting `fn` releases the key; the rejection reaches only that caller.
 * @returns {{ run: <T>(key: string, fn: () => Promise<T>|T) => Promise<T> }}
 */
export function createKeyedMutex() {
  /** @type {Map<string, Promise<void>>} */
  const tails = new Map();
  return {
    run(key, fn) {
      const prev = tails.get(key) ?? Promise.resolve();
      const result = prev.then(() => fn());
      const tail = result.then(() => {}, () => {});
      tails.set(key, tail);
      tail.then(() => {
        if (tails.get(key) === tail) tails.delete(key);
      });
      return result;
    },
  };
}

/** @returns {{ run: <T>(fn: () => Promise<T>|T) => Promise<T>, active: number, queued: number }} a counting semaphore */
export function createSemaphore(limit) {
  let active = 0;
  const queue = [];
  const release = () => {
    active--;
    const next = queue.shift();
    if (next) next();
  };
  const acquire = () => new Promise((resolve) => {
    const take = () => { active++; resolve(); };
    if (active < limit) take();
    else queue.push(take);
  });
  return {
    async run(fn) {
      await acquire();
      try {
        return await fn();
      } finally {
        release();
      }
    },
    get active() { return active; },
    get queued() { return queue.length; },
  };
}

/**
 * At most one call per `intervalMs` per key. Returns true when the call is allowed now.
 * Keeps a Map with lazy cleanup.
 * @param {number} intervalMs
 * @returns {(key: string) => boolean}
 */
export function createThrottle(intervalMs) {
  /** @type {Map<string, number>} */
  const last = new Map();
  let lastSweep = Date.now();
  return (key) => {
    const now = Date.now();
    if (now - lastSweep > intervalMs) {
      for (const [k, t] of last) if (now - t >= intervalMs) last.delete(k);
      lastSweep = now;
    }
    const prev = last.get(key);
    if (prev !== undefined && now - prev < intervalMs) return false;
    last.set(key, now);
    return true;
  };
}
