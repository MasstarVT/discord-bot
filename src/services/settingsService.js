// The only way to read or write GuildSettings (00-overview rule 8; 01-foundation §5).
// Rows are versioned in Redis, memoised per shard for 30 s, deep-frozen, and written through.
// Feature code never calls cache.del('guild:…:settings') itself.
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import logger from '../utils/logger.js';
import { createThrottle } from '../utils/async.js';

export const SETTINGS_TTL_SECONDS = 300;
export const SETTINGS_MEMO_TTL_MS = 30_000;
export const SETTINGS_DATE_FIELDS = Object.freeze(['createdAt', 'updatedAt']);
// Bump whenever the GuildSettings row shape changes (new columns, new defaults). Rows cached
// without this marker (written before the migration, by legacy code, or by a rolled-back
// build) are treated as cache misses.
export const SETTINGS_CACHE_VERSION = 2;
export const CACHE_VERSION_FIELD = '_cacheV';
export const settingsKey = (guildId) => `guild:${guildId}:settings`;
export { isModuleEnabled } from '../config/modules.js';

const MEMO_MAX = 10_000;
const WARN_INTERVAL_MS = 60_000;

/** guildId -> { row, expires }. Insertion order = age, so the first key is the oldest. */
const memo = new Map();
/** Guilds whose GuildSettings row is known to exist in this process. */
const ensured = new Set();
/** guildId -> number, bumped by every write/invalidate so a slow reader never memoises a stale row. */
const generations = new Map();
/** In-flight cold loads, so a burst of events for one guild shares one query. */
const loading = new Map();
const allowWarn = createThrottle(WARN_INTERVAL_MS);

const generation = (guildId) => generations.get(guildId) ?? 0;
function bumpGeneration(guildId) {
  generations.set(guildId, generation(guildId) + 1);
  if (generations.size > MEMO_MAX * 2) generations.delete(generations.keys().next().value);
}

function memoSet(guildId, row) {
  memo.delete(guildId);
  memo.set(guildId, { row, expires: Date.now() + SETTINGS_MEMO_TTL_MS });
  while (memo.size > MEMO_MAX) memo.delete(memo.keys().next().value);
}

function memoGet(guildId) {
  const entry = memo.get(guildId);
  if (!entry) return null;
  if (entry.expires <= Date.now()) {
    memo.delete(guildId);
    return null;
  }
  return entry.row;
}

function markEnsured(guildId) {
  ensured.add(guildId);
  if (ensured.size > MEMO_MAX * 2) ensured.delete(ensured.values().next().value);
}

const isPlainObject = (v) => {
  if (v === null || typeof v !== 'object') return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
};

function deepFreeze(value) {
  if (Array.isArray(value)) {
    for (const item of value) deepFreeze(item);
    return Object.freeze(value);
  }
  if (isPlainObject(value)) {
    for (const key of Object.keys(value)) deepFreeze(value[key]);
    return Object.freeze(value);
  }
  return value; // Dates and primitives
}

/**
 * Pure. True only when `hit` is a plain object for this guild written by the current row shape.
 * @param {any} hit value read from Redis
 * @param {string} guildId
 */
export function isValidCachedSettings(hit, guildId) {
  return isPlainObject(hit) && hit.guildId === guildId && hit[CACHE_VERSION_FIELD] === SETTINGS_CACHE_VERSION;
}

/**
 * Strips the cache marker, converts ISO strings in SETTINGS_DATE_FIELDS to Date and deep-freezes
 * the row, every array field and every plain object/array inside Json columns. The top-level
 * object is copied; nested values are frozen in place (they come from JSON.parse or a Prisma row
 * the caller no longer owns).
 * @param {object|null} row
 * @returns {Readonly<object>|null}
 */
export function reviveSettings(row) {
  if (!row || typeof row !== 'object') return null;
  const copy = { ...row };
  delete copy[CACHE_VERSION_FIELD];
  for (const field of SETTINGS_DATE_FIELDS) {
    if (typeof copy[field] === 'string') copy[field] = new Date(copy[field]);
  }
  return deepFreeze(copy);
}

const withMarker = (row) => ({ ...row, [CACHE_VERSION_FIELD]: SETTINGS_CACHE_VERSION });

async function loadSettings(guildId, key) {
  const gen = generation(guildId);
  const hit = await cache.get(key);
  if (isValidCachedSettings(hit, guildId)) {
    const row = reviveSettings(hit);
    if (generation(guildId) === gen) memoSet(guildId, row);
    return row;
  }
  // Old shape, wrong guild or no marker: drop it so the NX write below can replace it.
  if (hit !== null && hit !== undefined) await cache.del(key);

  let row = await prisma.guildSettings.findUnique({ where: { guildId } });
  if (!row) row = await prisma.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
  // NX: a reader can never overwrite a newer value written by updateSettings (R214).
  await cache.setNX(key, withMarker(row), SETTINGS_TTL_SECONDS);
  const revived = reviveSettings(row);
  if (generation(guildId) === gen) {
    memoSet(guildId, revived);
    markEnsured(guildId);
  }
  return revived;
}

/**
 * Loads a guild's settings: memo, then Redis, then Postgres (created on first use).
 * Redis failures fall through to Postgres. The result is deep-frozen and shared: never mutate it.
 * @param {string} guildId
 * @param {{ fresh?: boolean }} [opts] fresh skips the in-process memo
 * @returns {Promise<Readonly<object>>}
 * @throws only when Postgres fails
 */
export async function getSettings(guildId, { fresh = false } = {}) {
  if (!fresh) {
    const hit = memoGet(guildId);
    if (hit) return hit;
    const pending = loading.get(guildId);
    if (pending) return pending;
  }
  const key = settingsKey(guildId);
  const promise = loadSettings(guildId, key);
  if (!fresh) {
    loading.set(guildId, promise);
    const clear = () => { if (loading.get(guildId) === promise) loading.delete(guildId); };
    promise.then(clear, clear);
  }
  return promise;
}

/**
 * Like getSettings but never throws: logs a warning (at most one per guild per minute) and returns null.
 * @param {string} guildId
 * @returns {Promise<Readonly<object>|null>}
 */
export async function getSettingsSafe(guildId) {
  try {
    return await getSettings(guildId);
  } catch (err) {
    if (allowWarn(guildId)) logger.warn(`Could not load settings for guild ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

/**
 * Memo-only, synchronous, no I/O. For sync hook filters and acknowledgement-critical paths.
 * @param {string} guildId
 * @returns {Readonly<object>|null}
 */
export function peekSettings(guildId) {
  return memoGet(guildId);
}

/**
 * Makes sure the GuildSettings row exists (every per-guild model has an FK to it). Returns at
 * once when the guild was ensured in this process.
 * @param {string} guildId
 * @throws on DB failure
 */
export async function ensureGuildSettings(guildId) {
  if (ensured.has(guildId)) return;
  await prisma.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
  markEnsured(guildId);
}

function writeThrough(guildId, row) {
  bumpGeneration(guildId);
  const revived = reviveSettings(row);
  memoSet(guildId, revived);
  markEnsured(guildId);
  return revived;
}

/**
 * Upserts scalar GuildSettings fields and writes the result through to the memo and Redis
 * (a plain SET that overwrites). Use updateSettingsWith for arrays/JSON read-modify-write.
 * @param {string} guildId
 * @param {object} data GuildSettings scalar fields only
 * @returns {Promise<Readonly<object>>}
 * @throws on DB or validation failure
 */
export async function updateSettings(guildId, data) {
  const row = await prisma.guildSettings.upsert({
    where: { guildId },
    update: data,
    create: { guildId, ...data },
  });
  const cached = withMarker(row);
  const revived = writeThrough(guildId, row);
  await cache.set(settingsKey(guildId), cached, SETTINGS_TTL_SECONDS);
  return revived;
}

const LOCK_ROW = (guildId) => Prisma.sql`SELECT 1 FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'settings.lockRow', build: () => LOCK_ROW('0') },
]);

/**
 * Read-modify-write for arrays and JSON without lost updates: the row is locked (FOR UPDATE)
 * while `mutator(current)` runs. The mutator must be pure (no Discord calls, no slow awaits),
 * receives a fresh unfrozen copy of the row and returns the fields to change (falsy/empty = none).
 * An error thrown by the mutator rolls back and is re-thrown unchanged.
 * @param {string} guildId
 * @param {(current: object) => Promise<object|null|undefined>|object|null|undefined} mutator
 * @returns {Promise<Readonly<object>>}
 * @throws on DB failure or when the mutator throws
 */
export async function updateSettingsWith(guildId, mutator) {
  const row = await prisma.$transaction(async (tx) => {
    await tx.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
    await tx.$queryRaw(LOCK_ROW(guildId));
    const current = await tx.guildSettings.findUnique({ where: { guildId } });
    const data = await mutator(structuredClone(current));
    if (!data || Object.keys(data).length === 0) return current;
    return tx.guildSettings.update({ where: { guildId }, data });
  });
  const cached = withMarker(row);
  const revived = writeThrough(guildId, row);
  await cache.set(settingsKey(guildId), cached, SETTINGS_TTL_SECONDS);
  return revived;
}

/**
 * Drops the memo entry, the ensured flag and the Redis key (guildCreate / guildDelete). Never throws.
 * @param {string} guildId
 */
export async function invalidateSettings(guildId) {
  bumpGeneration(guildId);
  memo.delete(guildId);
  ensured.delete(guildId);
  loading.delete(guildId);
  await cache.del(settingsKey(guildId));
}
