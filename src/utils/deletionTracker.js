// ── Bot-caused deletion tracker ────────────────────────────────────────────────
// Records which message (or channel) deletions the bot caused, so log hooks can tell
// them apart from deletions by members. In-memory and per shard; no imports.

const MAX_ENTRIES = 10_000;
const SWEEP_AFTER_SIZE = 1_000;
const SWEEP_INTERVAL_MS = 60_000;
const DEFAULT_TTL_MS = 60_000;
const MAX_TTL_MS = 3_600_000;

/** @type {Map<string, { reason: string, expiresAt: number }>} insertion order = age order */
const store = new Map();
let lastSweep = 0;

function sweep(now) {
  lastSweep = now;
  for (const [id, entry] of store) {
    if (entry.expiresAt <= now) store.delete(id);
  }
}

/**
 * Marks message or channel ids as deleted by the bot. Never throws.
 * @param {string|Iterable<string>} messageIds - one id, an array, or any iterable (e.g. `collection.keys()`)
 * @param {string} [reason='bot'] - short tag read back by `wasDeletedByBot` (e.g. 'automod', 'purge')
 * @param {number} [ttlMs=60000] - how long the mark lives (1 ms .. 1 h)
 */
export function markBotDeletion(messageIds, reason, ttlMs = DEFAULT_TTL_MS) {
  try {
    let ids;
    if (typeof messageIds === 'string') ids = [messageIds];
    else if (messageIds != null && typeof messageIds[Symbol.iterator] === 'function') ids = [...messageIds];
    else return;

    const why = String(reason ?? 'bot');
    let ttl = Math.floor(Number(ttlMs));
    if (!Number.isFinite(ttl)) ttl = DEFAULT_TTL_MS;
    ttl = Math.min(MAX_TTL_MS, Math.max(1, ttl));

    const now = Date.now();
    for (const raw of ids) {
      if (raw == null) continue;
      const id = String(raw);
      if (!id) continue;
      store.delete(id); // refresh the age order
      store.set(id, { reason: why, expiresAt: now + ttl });
    }
    while (store.size > MAX_ENTRIES) store.delete(store.keys().next().value);
    if (store.size > SWEEP_AFTER_SIZE && now - lastSweep > SWEEP_INTERVAL_MS) sweep(now);
  } catch {
    // never throws
  }
}

/**
 * Returns the reason when the bot deleted this message or channel recently, else null.
 * Non-destructive: several log hooks may read the same mark. Never throws.
 * @param {string} messageId
 * @returns {string|null}
 */
export function wasDeletedByBot(messageId) {
  try {
    const id = String(messageId);
    const entry = store.get(id);
    if (!entry) return null;
    if (entry.expiresAt <= Date.now()) {
      store.delete(id);
      return null;
    }
    return entry.reason;
  } catch {
    return null;
  }
}
