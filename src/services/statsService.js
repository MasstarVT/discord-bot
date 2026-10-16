// Statistics channels (pkg-stat-channels): locked voice channels whose NAME shows a live count (members, humans, bots, online, peak
// online, channel counts, roles, role members, boosts). Constants, kind metadata, pure helpers, the refresh engine, the stats.refresh
// job body, the hook bodies and the CRUD the /stats command calls. Side-effect free at import: every map below starts empty and no
// timer, I/O or registration runs at module scope. Redis is deliberately not used (Postgres is authoritative; a guild's commands,
// events and jobs always run on the shard that owns it).
//
// Rename budget: Discord allows about 2 renames per 10 minutes per channel. Each counter is renamed at most once per
// MIN_RENAME_GAP_MS, tracked in StatChannel.lastRenamedAt and reserved atomically BEFORE the request, so a timed-out or rate-limited
// request still counts and the spacing survives restarts. A change that cannot be applied yet (the gap) is never dropped: the
// blocked channel's earliest retry time becomes the job's next run (computeNextRunAt), and a member event, an edit or a new counter
// that arrives while a run is in flight caps that next run too (notePullDuringRun + refreshJobHandler).
import { ChannelType, OverwriteType, PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import {
  ensureGuildSettings, getSettingsSafe, isModuleEnabled, peekSettings, updateSettings,
} from './settingsService.js';
import { cancelJobs, getJob, scheduleJob } from './scheduler.js';
import { LIMITS } from '../config/constants.js';
import {
  TimeoutError, createKeyedMutex, createThrottle, withTimeout,
} from '../utils/async.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { clampText, formatNumber } from '../utils/text.js';
import { missingBotPermissions } from '../utils/interactionHelpers.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import { fetchAllMembers } from '../utils/guildMembers.js';
import { presencesEnabled } from '../utils/gatewayFeatures.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const REFRESH_INTERVAL_MS = 10 * 60_000;
export const MIN_RENAME_GAP_MS = 5 * 60_000;
export const MEMBER_PULL_DELAY_MS = 60_000;      // coalesced pull after a join or leave
export const MIN_NEXT_RUN_MS = 60_000;           // never reschedule sooner than this
export const EDIT_PULL_DELAY_MS = 5_000;
export const STARTUP_DELAY_MS = 30_000;          // guildReady: first run at +30 s ... +120 s
export const STARTUP_JITTER_MS = 90_000;
export const REFRESH_BUDGET_MS = 40_000;         // no new rename starts after this (the job timeout is 60 s)
export const RENAME_TIMEOUT_MS = 10_000;
export const MEMBER_FETCH_TIMEOUT_MS = 20_000;   // fetchAllMembers timeoutMs
export const MEMBER_FETCH_RETRY_MS = 5 * 60_000;
export const GUILD_FETCH_TIMEOUT_MS = 8_000;
export const CHANNEL_FETCH_TIMEOUT_MS = 5_000;   // rule 20: the REST check of a channel missing from the cache
export const PRESENCE_CACHE_MAX_MEMBERS = 75_000; // above this Discord sends no presences at startup
export const MEMBER_LIST_MAX_MEMBERS = 100_000;  // HUMANS/BOTS/ROLE_MEMBERS refused above this
export const MAX_NAME_LENGTH = 100;              // Discord channel-name limit
export const WORST_CASE_COUNT = 2_147_483_647;
export const ROWS_MEMO_TTL_MS = 15 * 60_000;
export const WARN_THROTTLE_MS = 6 * 60 * 60_000;
export const MEMO_CAP = 10_000;
export const RENAME_REASON = 'Statistics channel update';
const UNKNOWN_RETRY_MS = 2 * 60_000;             // a channel that could not be checked is looked at again after this
const PULL_FLOOR_MS = 5_000;                     // a pull claimed during a run is honoured no sooner than now + this
const INT_MAX = 2_147_483_647;

// ── Kinds ─────────────────────────────────────────────────────────────────────

/** = the Prisma enum StatKind, same order. */
export const STAT_KINDS = Object.freeze(['MEMBERS', 'HUMANS', 'BOTS', 'ONLINE', 'PEAK_ONLINE', 'TEXT_CHANNELS',
  'VOICE_CHANNELS', 'TOTAL_CHANNELS', 'ROLES', 'ROLE_MEMBERS', 'BOOSTS']);
export const KIND_INFO = Object.freeze({
  MEMBERS: Object.freeze({ label: 'Members', template: 'Members: {count}' }),
  HUMANS: Object.freeze({ label: 'Humans', template: 'Humans: {count}' }),
  BOTS: Object.freeze({ label: 'Bots', template: 'Bots: {count}' }),
  ONLINE: Object.freeze({ label: 'Online', template: 'Online: {count}' }),
  PEAK_ONLINE: Object.freeze({ label: 'Peak online', template: 'Peak Online: {count}' }),
  TEXT_CHANNELS: Object.freeze({ label: 'Text channels', template: 'Text Channels: {count}' }),
  VOICE_CHANNELS: Object.freeze({ label: 'Voice channels', template: 'Voice Channels: {count}' }),
  TOTAL_CHANNELS: Object.freeze({ label: 'All channels', template: 'Channels: {count}' }),
  ROLES: Object.freeze({ label: 'Roles', template: 'Roles: {count}' }),
  ROLE_MEMBERS: Object.freeze({ label: 'Role members', template: '{role}: {count}' }),
  BOOSTS: Object.freeze({ label: 'Boosts', template: 'Boosts: {count}' }),
});
// A Set cannot really be frozen; Object.freeze is cosmetic. These sets are never mutated.
export const LIST_KINDS = Object.freeze(new Set(['HUMANS', 'BOTS', 'ROLE_MEMBERS'])); // need the full member list
export const ONLINE_KINDS = Object.freeze(new Set(['ONLINE', 'PEAK_ONLINE']));
export const MEMBER_EVENT_KINDS = Object.freeze(new Set(['MEMBERS', 'HUMANS', 'BOTS', 'ONLINE', 'PEAK_ONLINE', 'ROLE_MEMBERS']));
export const TEXT_TYPES = Object.freeze(new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildMedia]));
export const VOICE_TYPES = Object.freeze(new Set([ChannelType.GuildVoice, ChannelType.GuildStageVoice]));
export const PROBLEMS = Object.freeze({
  'members-incomplete': 'Loading the member list; the count will update shortly',
  'server-too-large': 'This server is too large for member-list counters',
  'role-deleted': 'The counted role was deleted; remove this counter with /stats delete',
  'presence-unavailable': 'Online data is unavailable right now',
  'missing-permissions': 'I need View Channel and Manage Channels on this channel',
  // Rule 20: a channel missing from the cache is not proof of deletion; it is removed only after Discord confirms (10003).
  'channel-deleted': "I can't find this channel; the counter is removed once Discord confirms it was deleted",
  'channel-unknown': 'I could not check this channel just now; it will be tried again',
  error: 'The last update failed; it will be retried',
});

/** @param {string} code @returns {string} */
export function describeProblem(code) {
  return Object.hasOwn(PROBLEMS, code) ? PROBLEMS[code] : PROBLEMS.error;
}

export class StatsError extends Error {
  /** @param {string} code LIMIT, ROLE_LIMIT, DUPLICATE (channelId), MISSING_PERMISSIONS, CHANNEL_LIMIT, DISCORD_REJECTED, NOT_FOUND */
  constructor(code, extra = {}) {
    super(code);
    this.name = 'StatsError';
    this.code = code;
    Object.assign(this, extra);
  }
}

// ── In-process state (per shard; empty after a restart) ───────────────────────

/** guildId -> { at, rows: StatRowLite[] }. Cheap sync filters for the member and channel hooks. */
const rowsMemo = new Map();
/** guildId -> epoch ms the job was pulled to (coalesces member-event pulls). */
const pendingPull = new Map();
/** One refresh per guild at a time. */
const busyGuilds = new Set();
/** guildId -> epoch ms before which the full member list is not requested again. */
const fetchBlockedUntil = new Map();
/** channelId -> names Discord normalised on rename, so a normalised name never causes a rename loop. */
const nameAliases = new Map();
/** "Missing Permissions logs once and skips" (one line per channel and problem per 6 h). Recreated by __resetState. */
let warnOnce = createThrottle(WARN_THROTTLE_MS);
/** Serialises create, edit and delete per guild so the caps and duplicate checks cannot race. Recreated by __resetState. */
let configMutex = createKeyedMutex();

/** Map.set with the MEMO_CAP bound: the oldest insertion is evicted first. */
function memoSet(map, key, value) {
  map.delete(key);
  map.set(key, value);
  while (map.size > MEMO_CAP) map.delete(map.keys().next().value);
}

const lite = (r) => ({ id: r.id, channelId: r.channelId, kind: r.kind, roleId: r.roleId ?? null });
const primeMemo = (guildId, rows) => memoSet(rowsMemo, guildId, { at: Date.now(), rows: rows.map(lite) });

function memoAddRow(guildId, row) {
  const m = rowsMemo.get(guildId);
  if (m) m.rows = [...m.rows.filter((r) => r.id !== row.id), lite(row)];
}

function forgetRow(guildId, rowId) {
  const m = rowsMemo.get(guildId);
  if (m) m.rows = m.rows.filter((r) => r.id !== rowId);
}

const freshMemo = (guildId) => {
  const m = rowsMemo.get(guildId);
  return m && Date.now() - m.at <= ROWS_MEMO_TTL_MS ? m : null;
};

/** Test-only. */
export function __setRowsMemo(guildId, rows, at = Date.now()) {
  memoSet(rowsMemo, guildId, { at, rows: rows.map(lite) });
}

/** Test-only: clears every in-process map and set and recreates the throttle and the mutex. */
export function __resetState() {
  rowsMemo.clear();
  pendingPull.clear();
  busyGuilds.clear();
  fetchBlockedUntil.clear();
  nameAliases.clear();
  warnOnce = createThrottle(WARN_THROTTLE_MS);
  configMutex = createKeyedMutex();
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** @returns {number} an integer in 0 ... 2_147_483_647 (the Int column range) */
export function clampInt(n) {
  return Math.max(0, Math.min(INT_MAX, Math.trunc(Number(n) || 0)));
}

export const refreshJobKey = (guildId) => `stats.refresh:${guildId}`;

export function normalizeTemplate(str) {
  return String(str ?? '').replace(/\s+/g, ' ').trim();
}

/**
 * The channel name for a template and a count. A null count renders as an ellipsis (creation only).
 * @param {string} template
 * @param {{ count?: number|null, roleName?: string|null, guild?: object|null }} [opts]
 * @returns {string} 1-100 characters
 */
export function renderStatName(template, { count, roleName = null, guild = null } = {}) {
  const vars = { count: Number.isFinite(count) ? formatNumber(clampInt(count)) : '…' };
  if (roleName != null) vars.role = String(roleName);
  const text = renderPlaceholders(template, { guild, vars });
  return clampText(normalizeTemplate(text), MAX_NAME_LENGTH) || vars.count;
}

/**
 * Validates and normalises a name template.
 * @param {string} input
 * @param {{ kind: string, roleName?: string|null, guild?: object|null }} opts
 * @returns {{ ok: true, template: string } | { ok: false, error: string }}
 */
export function validateStatTemplate(input, { kind, roleName = null, guild = null } = {}) {
  const t = normalizeTemplate(input);
  if (t.length < 1 || t.length > MAX_NAME_LENGTH) return { ok: false, error: 'Templates must be 1–100 characters.' };
  if (!/\{count\}/i.test(t)) return { ok: false, error: 'The template must contain `{count}`, e.g. `Members: {count}`.' };
  if (/\{random/i.test(t)) return { ok: false, error: 'Random variables are not allowed: the name would change on every refresh.' };
  if (/\{role\}/i.test(t) && kind !== 'ROLE_MEMBERS') {
    return { ok: false, error: '`{role}` only works with the **Role members** counter.' };
  }
  const vars = {
    count: formatNumber(WORST_CASE_COUNT),
    ...(kind === 'ROLE_MEMBERS' ? { role: roleName ?? 'role' } : {}),
  };
  // Tokens come from the TEMPLATE, not the output, so braces inside a role or server name never cause a false error.
  for (const m of t.matchAll(/\{[^{}\n]{1,64}\}/g)) {
    const whole = m[0];
    if (renderPlaceholders(whole, { guild, vars }) === whole) {
      return { ok: false, error: `Unknown variable \`${whole}\`. Use \`{count}\`, \`{role}\` (role counters) or server variables such as \`{server}\`.` };
    }
  }
  const len = normalizeTemplate(renderPlaceholders(t, { guild, vars })).length;
  if (len > MAX_NAME_LENGTH) {
    return { ok: false, error: `With a large count the name would be ${len} characters; keep it to 100 or fewer.` };
  }
  return { ok: true, template: t };
}

/** @returns {boolean} whether the member cache holds every member (a count from a partial cache is never shown) */
export function isMemberCacheComplete(guild) {
  return guild.members.cache.size >= guild.memberCount;
}

/** @returns {{ text: number, voice: number, total: number }} threads are skipped; total excludes categories */
export function countChannels(channels) {
  let text = 0;
  let voice = 0;
  let total = 0;
  for (const c of channels.values()) {
    if (c.isThread?.()) continue;
    if (TEXT_TYPES.has(c.type)) text++;
    if (VOICE_TYPES.has(c.type)) voice++;
    if (c.type !== ChannelType.GuildCategory) total++;
  }
  return { text, voice, total };
}

/**
 * Everything a counter can show, read from the caches only (synchronous).
 * @param {object} guild
 * @param {{ roleIds?: string[], approxOnline?: number|null, presences?: boolean }} [opts] presences: false when the bot runs
 *   without the Presence intent (DISABLE_PRESENCES), so the online count is always Discord's approximate one
 */
export function buildSnapshot(guild, { roleIds = [], approxOnline = null, presences = presencesEnabled() } = {}) {
  const members = Number(guild.memberCount ?? 0);
  const membersComplete = isMemberCacheComplete(guild);
  let humans = null;
  let bots = null;
  const roleCounts = new Map();
  if (membersComplete && members <= MEMBER_LIST_MAX_MEMBERS) {
    humans = 0;
    bots = 0;
    for (const id of roleIds) roleCounts.set(id, 0);
    for (const m of guild.members.cache.values()) {
      if (m.user?.bot) bots++;
      else humans++;
      for (const id of roleIds) if (m.roles?.cache?.has(id)) roleCounts.set(id, roleCounts.get(id) + 1);
    }
  }
  let online;
  if (presences && members <= PRESENCE_CACHE_MAX_MEMBERS) {
    online = 0;
    for (const p of guild.presences?.cache?.values() ?? []) if (p.status !== 'offline') online++;
  } else {
    online = approxOnline ?? null;
  }
  const { text, voice, total } = countChannels(guild.channels.cache);
  return {
    members,
    membersComplete,
    humans,
    bots,
    online,
    text,
    voice,
    total,
    roles: Math.max(0, guild.roles.cache.size - 1), // minus @everyone
    boosts: guild.premiumSubscriptionCount ?? 0,
    roleCounts,
    rolesMissing: new Set(roleIds.filter((id) => !guild.roles.cache.has(id))),
  };
}

/**
 * The value a row should show. `value` is null when it cannot be known right now; then `problem` says why.
 * @returns {{ value: number|null, problem: string|null }}
 */
export function computeValue(kind, snap, row) {
  const ok = (v) => ({ value: clampInt(v), problem: null });
  const bad = (p) => ({ value: null, problem: p });
  switch (kind) {
    case 'MEMBERS': return ok(snap.members);
    case 'HUMANS':
    case 'BOTS':
      if (snap.members > MEMBER_LIST_MAX_MEMBERS) return bad('server-too-large');
      if (!snap.membersComplete) return bad('members-incomplete');
      return ok(kind === 'HUMANS' ? snap.humans : snap.bots);
    case 'ONLINE': return snap.online == null ? bad('presence-unavailable') : ok(snap.online);
    case 'PEAK_ONLINE': {
      const prev = row?.peakValue ?? null;
      if (snap.online == null) return prev == null ? bad('presence-unavailable') : ok(prev);
      return ok(Math.max(prev ?? 0, snap.online));
    }
    case 'TEXT_CHANNELS': return ok(snap.text);
    case 'VOICE_CHANNELS': return ok(snap.voice);
    case 'TOTAL_CHANNELS': return ok(snap.total);
    case 'ROLES': return ok(snap.roles);
    case 'ROLE_MEMBERS':
      if (!row?.roleId || snap.rolesMissing.has(row.roleId)) return bad('role-deleted');
      if (snap.members > MEMBER_LIST_MAX_MEMBERS) return bad('server-too-large');
      if (!snap.membersComplete) return bad('members-incomplete');
      return ok(snap.roleCounts.get(row.roleId) ?? 0);
    case 'BOOSTS': return ok(snap.boosts);
    default: return bad('error');
  }
}

/**
 * Whether the channel may be renamed now. 'same' also covers a name Discord normalised on an earlier rename (alias).
 * @returns {{ action: 'same' } | { action: 'rename' } | { action: 'wait', retryAt: number }}
 */
export function decideRename({
  desiredName, currentName, alias = null, lastRenamedAt = null, now = Date.now(),
}) {
  if (currentName === desiredName) return { action: 'same' };
  if (alias && alias.desired === desiredName && alias.actual === currentName) return { action: 'same' };
  if (lastRenamedAt == null || now - lastRenamedAt.getTime() >= MIN_RENAME_GAP_MS) return { action: 'rename' };
  return { action: 'wait', retryAt: lastRenamedAt.getTime() + MIN_RENAME_GAP_MS };
}

/**
 * When the job should run next: every REFRESH_INTERVAL_MS, earlier when a rename becomes allowed (+5 s) or when `soon`,
 * and never sooner than MIN_NEXT_RUN_MS from now.
 * @returns {Date}
 */
export function computeNextRunAt({ now = Date.now(), retryAts = [], soon = false } = {}) {
  let t = now + REFRESH_INTERVAL_MS;
  for (const r of retryAts) t = Math.min(t, r + 5_000);
  if (soon) t = Math.min(t, now + MIN_NEXT_RUN_MS);
  t = Math.max(t, now + MIN_NEXT_RUN_MS);
  return new Date(t);
}

/** @returns {string|null} the channel id of a mention or a raw id */
export function parseChannelRef(input) {
  const m = /^(?:<#)?(\d{17,20})>?$/.exec(String(input ?? '').trim());
  return m ? m[1] : null;
}

/** @ everyone cannot connect; the bot keeps View Channel and Manage Channels (Discord lets a bot set only permissions it holds). */
export function buildOverwrites(guild, botId) {
  return [
    { id: guild.id, type: OverwriteType.Role, deny: [PermissionFlagsBits.Connect] },
    { id: botId, type: OverwriteType.Member, allow: [PermissionFlagsBits.ViewChannel, PermissionFlagsBits.ManageChannels] },
  ];
}

/**
 * Coalesces member-event pulls: true when the caller should pull the job now, false when a pull is already pending
 * (that run will see this change too).
 */
export function claimPull(guildId, now = Date.now()) {
  const t = pendingPull.get(guildId);
  if (t && t > now) return false;
  memoSet(pendingPull, guildId, now + MEMBER_PULL_DELAY_MS);
  return true;
}

/**
 * A pull or an ensure that arrives while a refresh is in flight cannot move the job: it is RUNNING with a past runAt, so
 * scheduleJob('earliest') keeps it, and the run's own settle then writes the next run (up to 10 minutes away). Whatever the run
 * snapshotted may predate the change (an edited template, a new counter, a join), so the request is remembered in pendingPull and
 * refreshJobHandler caps its next run at that time. A no-op when no refresh is running.
 */
function notePullDuringRun(guildId, runAtMs) {
  if (!busyGuilds.has(guildId)) return;
  const prev = pendingPull.get(guildId);
  if (prev === undefined || prev <= Date.now() || runAtMs < prev) memoSet(pendingPull, guildId, runAtMs);
}

/** Discord REST error code from a discord.js error, or null. */
function discordErrorCode(err) {
  if (typeof err?.code === 'number') return err.code;
  const raw = err?.rawError?.code;
  return typeof raw === 'number' ? raw : null;
}

/**
 * One refresh per guild at a time.
 * @returns {Promise<{ busy: true } | { busy: false, value: any }>}
 */
export async function runExclusive(guildId, fn) {
  if (busyGuilds.has(guildId)) return { busy: true };
  busyGuilds.add(guildId);
  try {
    return { busy: false, value: await fn() };
  } finally {
    busyGuilds.delete(guildId);
  }
}

// ── Rule 20: a cache miss is not a deletion ───────────────────────────────────

/**
 * Whether a channel exists, decided by the cache or a REST fetch, never by a cache miss alone.
 * `gone` only after a REST 10003 on an available guild; `unknown` means: keep the row.
 * @returns {Promise<{ state: 'present', channel: object } | { state: 'gone', channel: null } | { state: 'unknown', channel: null, code?: number|null }>}
 */
export async function confirmChannel(guild, channelId) {
  const cached = guild.channels.cache.get(channelId);
  if (cached) return { state: 'present', channel: cached };
  if (!guild.available) return { state: 'unknown', channel: null };
  try {
    const fetched = await withTimeout(guild.channels.fetch(channelId), CHANNEL_FETCH_TIMEOUT_MS, `stats channel fetch ${channelId}`);
    return fetched ? { state: 'present', channel: fetched } : { state: 'unknown', channel: null };
  } catch (err) {
    const code = discordErrorCode(err);
    if (code === 10003) return { state: 'gone', channel: null };
    return { state: 'unknown', channel: null, code };
  }
}

// ── Database reads ────────────────────────────────────────────────────────────

/** Every row of a guild, oldest first; primes the memo. Throws on a DB error. */
export async function loadRows(guildId) {
  const rows = await prisma.statChannel.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' } });
  primeMemo(guildId, rows);
  return rows;
}

export const listRows = loadRows;

/**
 * Autocomplete rows: at most 25, bounded in time, never throws.
 * @returns {Promise<Array<{ channelId: string, kind: string, roleId: string|null }>>} [] on error
 */
export async function listRowsSafe(guildId, { timeoutMs = 2_000 } = {}) {
  try {
    return await withTimeout(
      prisma.statChannel.findMany({
        where: { guildId }, orderBy: { createdAt: 'asc' }, take: 25, select: { channelId: true, kind: true, roleId: true },
      }),
      timeoutMs,
      'stats autocomplete',
    );
  } catch (err) {
    logger.warn(`stats autocomplete lookup failed for guild ${guildId}: ${err?.message ?? err}`);
    return [];
  }
}

/** The memo when fresh, else the database. null on a DB error (the hook then fails closed). */
export async function getGuildRows(guildId) {
  const m = freshMemo(guildId);
  if (m) return m.rows;
  try {
    return (await loadRows(guildId)).map(lite);
  } catch (err) {
    logger.error(`stats: could not load the counters of guild ${guildId}`, err);
    return null;
  }
}

/**
 * Finds a counter by channel mention, channel id or typed channel name. Never throws.
 * @returns {Promise<object|null>}
 */
export async function findRowByRef(guild, input) {
  try {
    const id = parseChannelRef(input);
    if (id) return await prisma.statChannel.findFirst({ where: { guildId: guild.id, channelId: id } });
    const wanted = String(input ?? '').trim().toLowerCase();
    if (!wanted) return null;
    const rows = await listRows(guild.id);
    return rows.find((r) => guild.channels.cache.get(r.channelId)?.name?.toLowerCase() === wanted) ?? null;
  } catch (err) {
    logger.warn(`stats: counter lookup failed in guild ${guild?.id}: ${err?.message ?? err}`);
    return null;
  }
}

/**
 * The first problem with a counter, from the caches only (display; never deletes anything).
 * @returns {string|null} a PROBLEMS key
 */
export function inspectRow(guild, row) {
  const channel = guild.channels.cache.get(row.channelId);
  if (!channel) return 'channel-deleted';
  if (missingBotPermissions(channel, ['ViewChannel', 'ManageChannels']).length) return 'missing-permissions';
  if (row.kind === 'ROLE_MEMBERS' && !guild.roles.cache.has(row.roleId)) return 'role-deleted';
  if (LIST_KINDS.has(row.kind) && guild.memberCount > MEMBER_LIST_MAX_MEMBERS) return 'server-too-large';
  return null;
}

// ── Collecting a snapshot (never throws) ──────────────────────────────────────

/**
 * Loads what the rows need (the full member list, Discord's approximate online count) and builds the snapshot.
 * @returns {Promise<{ snap: object, retryAt: number|null }>} retryAt: when to look again for an incomplete member list
 */
export async function collectSnapshot(guild, rows, { allowFetch = true } = {}) {
  const needList = rows.some((r) => LIST_KINDS.has(r.kind));
  const needOnline = rows.some((r) => ONLINE_KINDS.has(r.kind));
  const roleIds = [...new Set(rows.filter((r) => r.kind === 'ROLE_MEMBERS' && r.roleId).map((r) => r.roleId))];
  const members = Number(guild.memberCount ?? 0);
  const listable = needList && members <= MEMBER_LIST_MAX_MEMBERS;

  if (listable && !isMemberCacheComplete(guild) && allowFetch && Date.now() >= (fetchBlockedUntil.get(guild.id) ?? 0)) {
    // Claim first: the claim also stops concurrent fetches. It is lifted only by a fetch that completed the cache.
    memoSet(fetchBlockedUntil, guild.id, Date.now() + MEMBER_FETCH_RETRY_MS);
    try {
      const res = await fetchAllMembers(guild, { timeoutMs: MEMBER_FETCH_TIMEOUT_MS, maxMembers: MEMBER_LIST_MAX_MEMBERS });
      if (res.ok) {
        if (isMemberCacheComplete(guild)) fetchBlockedUntil.delete(guild.id);
      } else if (res.reason === 'rate-limited') {
        // Opcode-8 rate limit: back off for Discord's retry_after (at least 30 s).
        memoSet(fetchBlockedUntil, guild.id, Date.now() + Math.max(30_000, Math.ceil(res.retryAfterMs ?? 30_000)));
        if (warnOnce(`${guild.id}:fetch`)) logger.warn(`Stat channels: the member list of guild ${guild.id} is rate limited; trying again later`);
      }
      // timeout / too-large / error: keep the 5-minute claim. A timed-out fetch keeps running and the next run usually finds a
      // complete cache; fetchAllMembers already logged the error case.
    } catch (err) {
      logger.warn(`Stat channels: member fetch failed for guild ${guild.id}: ${err?.message ?? err}`);
    }
  }

  let retryAt = null;
  if (listable && !isMemberCacheComplete(guild)) {
    retryAt = Math.max(Date.now() + MIN_NEXT_RUN_MS, fetchBlockedUntil.get(guild.id) ?? 0);
  }

  let approxOnline = null;
  const presences = presencesEnabled();
  if (needOnline && (!presences || members > PRESENCE_CACHE_MAX_MEMBERS)) {
    try {
      const g = await withTimeout(guild.fetch(), GUILD_FETCH_TIMEOUT_MS, 'stats guild fetch');
      approxOnline = g?.approximatePresenceCount ?? null;
    } catch (err) {
      if (warnOnce(`${guild.id}:guildfetch`)) logger.warn(`Stat channels: could not read the online count of guild ${guild.id}: ${err?.message ?? err}`);
      approxOnline = null;
    }
  }

  return { snap: buildSnapshot(guild, { roleIds, approxOnline, presences }), retryAt };
}

// ── The refresh engine ────────────────────────────────────────────────────────

const newResult = () => ({
  renamed: 0, unchanged: 0, removed: 0, deferred: 0, waiting: [], skipped: [], nextRunAt: new Date(Date.now() + REFRESH_INTERVAL_MS),
});

async function dropRow(guild, row, why) {
  await prisma.statChannel.deleteMany({ where: { id: row.id } });
  forgetRow(guild.id, row.id);
  logger.info(`Stat channel ${row.channelId} ${why} in ${guild.id}; counter removed`);
}

/**
 * Recomputes every counter of a guild and renames the channels whose name changed (never more often than MIN_RENAME_GAP_MS per
 * channel). Never throws; each step is wrapped. Call through runExclusive.
 * @param {object} guild
 * @param {object[]} rows StatChannel rows, oldest first
 * @param {{ allowFetch?: boolean, startedAt?: number }} [opts]
 */
export async function refreshGuild(guild, rows, { allowFetch = true, startedAt = Date.now() } = {}) {
  const result = newResult();
  let soon = false;
  const extraRetries = [];
  let fetchRetryAt = null;
  try {
    if (!guild.available) {
      result.nextRunAt = new Date(Date.now() + 5 * 60_000);
      return result;
    }
    let snap;
    try {
      ({ snap, retryAt: fetchRetryAt } = await collectSnapshot(guild, rows, { allowFetch }));
    } catch (err) {
      logger.error(`stats refresh: snapshot failed in guild ${guild.id}`, err);
      for (const row of rows) result.skipped.push({ channelId: row.channelId, problem: 'error' });
      result.nextRunAt = computeNextRunAt({ now: Date.now(), soon: true });
      return result;
    }

    if (!guild.members.me) {
      for (const row of rows) result.skipped.push({ channelId: row.channelId, problem: 'error' });
      result.nextRunAt = computeNextRunAt({ now: Date.now(), soon: true });
      return result;
    }

    for (const row of rows) {
      try {
        // Budget: a run starts no rename after REFRESH_BUDGET_MS, so it ends inside the scheduler's 60 s timeout.
        if (Date.now() - startedAt > REFRESH_BUDGET_MS) {
          result.deferred++;
          soon = true;
          continue;
        }

        // Rule 20: a channel missing from the cache is NOT deleted. Discord is asked; only a 10003 removes the row.
        let channel = guild.channels.cache.get(row.channelId);
        if (!channel) {
          const chk = await confirmChannel(guild, row.channelId);
          if (chk.state === 'gone') {
            await dropRow(guild, row, 'no longer exists');
            result.removed++;
            continue;
          }
          if (chk.state === 'unknown') {
            const perm = chk.code === 50001 || chk.code === 50013;
            result.skipped.push({ channelId: row.channelId, problem: perm ? 'missing-permissions' : 'channel-unknown' });
            if (!perm) extraRetries.push(Date.now() + UNKNOWN_RETRY_MS);
            if (warnOnce(`${row.channelId}:lookup`)) {
              logger.warn(`Stat channel ${row.channelId} (${guild.id}): could not be checked${chk.code ? ` (Discord ${chk.code})` : ''}; the counter is kept`);
            }
            continue;
          }
          channel = chk.channel;
        }

        const { value, problem } = computeValue(row.kind, snap, row);

        // Peak: only ever increases; written even when the rename is blocked later on.
        if (row.kind === 'PEAK_ONLINE' && value != null && value > (row.peakValue ?? -1)) {
          await prisma.statChannel.updateMany({
            where: { id: row.id, OR: [{ peakValue: null }, { peakValue: { lt: value } }] },
            data: { peakValue: value },
          });
        }

        if (value == null) {
          result.skipped.push({ channelId: row.channelId, problem });
          if (problem === 'members-incomplete') soon = true;
          if ((problem === 'role-deleted' || problem === 'server-too-large') && warnOnce(`${row.channelId}:${problem}`)) {
            logger.warn(`Stat channel ${row.channelId} (${guild.id}): ${describeProblem(problem)}`);
          }
          continue;
        }

        const roleName = row.roleId ? (guild.roles.cache.get(row.roleId)?.name ?? null) : null;
        const desired = renderStatName(row.template, { count: value, roleName, guild });
        const d = decideRename({
          desiredName: desired,
          currentName: channel.name,
          alias: nameAliases.get(channel.id) ?? null,
          lastRenamedAt: row.lastRenamedAt ?? null,
          now: Date.now(),
        });
        if (d.action === 'same') {
          result.unchanged++;
          if (row.lastValue !== value) await prisma.statChannel.updateMany({ where: { id: row.id }, data: { lastValue: value } });
          continue;
        }
        if (d.action === 'wait') {
          result.waiting.push({ channelId: row.channelId, retryAt: d.retryAt });
          continue;
        }

        const missing = missingBotPermissions(channel, ['ViewChannel', 'ManageChannels']);
        if (missing.length) {
          result.skipped.push({ channelId: row.channelId, problem: 'missing-permissions' });
          if (warnOnce(`${channel.id}:perm`)) logger.warn(`Stat channel ${channel.id} (${guild.id}): missing ${missing.join(', ')}; skipping`);
          continue;
        }

        // Reserve the rename slot atomically, BEFORE the request: a timed-out or rate-limited request still counts against the
        // gap, the spacing survives restarts, and a concurrent caller cannot rename the same channel twice.
        const { count } = await prisma.statChannel.updateMany({
          where: { id: row.id, OR: [{ lastRenamedAt: null }, { lastRenamedAt: { lte: new Date(Date.now() - MIN_RENAME_GAP_MS) } }] },
          data: { lastRenamedAt: new Date() },
        });
        if (count === 0) {
          result.waiting.push({ channelId: row.channelId, retryAt: Date.now() + MIN_RENAME_GAP_MS });
          continue;
        }

        try {
          const updated = await withTimeout(channel.setName(desired, RENAME_REASON), RENAME_TIMEOUT_MS, `stats rename ${channel.id}`);
          if (typeof updated?.name === 'string' && updated.name !== desired) {
            // Discord normalised the name; remember it so the next run does not rename in a loop.
            memoSet(nameAliases, channel.id, { desired, actual: updated.name });
          }
          await prisma.statChannel.updateMany({ where: { id: row.id }, data: { lastValue: value } });
          result.renamed++;
        } catch (err) {
          const code = discordErrorCode(err);
          if (err instanceof TimeoutError) {
            // Probably queued behind a Discord rate limit and discord.js keeps it queued. The REST queue is sequential per
            // route, so the newest queued name lands last; the reservation keeps us from piling on.
            result.waiting.push({ channelId: row.channelId, retryAt: Date.now() + MIN_RENAME_GAP_MS });
          } else if (code === 10003) {
            await dropRow(guild, row, 'was deleted (Discord 10003 on rename)');
            result.removed++;
          } else if (code === 50013 || code === 50001) {
            result.skipped.push({ channelId: row.channelId, problem: 'missing-permissions' });
            if (warnOnce(`${channel.id}:perm`)) logger.warn(`Stat channel ${channel.id} (${guild.id}): Discord refused the rename (${code}); skipping`);
          } else if (err?.name === 'RateLimitError') {
            result.waiting.push({ channelId: row.channelId, retryAt: Date.now() + (Number(err.retryAfter) || MIN_RENAME_GAP_MS) });
          } else {
            result.skipped.push({ channelId: row.channelId, problem: 'error' });
            logger.error(`Stat channel ${channel.id} (${guild.id}): rename failed`, err);
          }
        }
      } catch (err) {
        result.skipped.push({ channelId: row.channelId, problem: 'error' });
        logger.error('stats refresh row failed', err);
      }
    }
  } catch (err) {
    logger.error(`stats refresh failed in guild ${guild?.id}`, err);
    soon = true;
  }

  result.nextRunAt = computeNextRunAt({
    now: Date.now(),
    retryAts: [...result.waiting.map((w) => w.retryAt), ...extraRetries, ...(fetchRetryAt ? [fetchRetryAt] : [])],
    soon,
  });
  return result;
}

// ── Scheduling ────────────────────────────────────────────────────────────────

/**
 * Makes sure the recurring job exists. A healthy job is only ever pulled EARLIER ('earliest'); a FAILED, DONE or CANCELLED
 * row is revived ('replace'). Never throws.
 * @returns {Promise<boolean>} true on success
 */
export async function ensureRefreshJob(guildId, runAt = new Date(Date.now() + REFRESH_INTERVAL_MS)) {
  try {
    notePullDuringRun(guildId, new Date(runAt).getTime());
    const job = await getJob(refreshJobKey(guildId));
    const healthy = Boolean(job) && (job.status === 'PENDING' || job.status === 'RUNNING');
    await scheduleJob({
      guildId, type: 'stats.refresh', runAt, payload: {}, dedupeKey: refreshJobKey(guildId), onConflict: healthy ? 'earliest' : 'replace',
    });
    return true;
  } catch (err) {
    logger.error(`stats: could not schedule the refresh of guild ${guildId}`, err);
    return false;
  }
}

/**
 * Pulls the job forward to now + delayMs ('earliest' never pushes it later). A job that is RUNNING has a past runAt, so it is kept;
 * the request is then remembered in pendingPull and refreshJobHandler honours it when it picks the next run. Never throws.
 * @returns {Promise<boolean>}
 */
export async function pullRefresh(guildId, delayMs = MEMBER_PULL_DELAY_MS) {
  try {
    notePullDuringRun(guildId, Date.now() + delayMs);
    await scheduleJob({
      guildId, type: 'stats.refresh', runAt: new Date(Date.now() + delayMs), payload: {}, dedupeKey: refreshJobKey(guildId), onConflict: 'earliest',
    });
    return true;
  } catch (err) {
    pendingPull.delete(guildId); // so a later event retries
    logger.error(`stats: could not pull the refresh of guild ${guildId}`, err);
    return false;
  }
}

export function cancelRefreshJob(guildId) {
  return cancelJobs({ dedupeKey: refreshJobKey(guildId) });
}

/**
 * The stats.refresh job body. Never throws, so the recurring job cannot go FAILED through handler errors. Idempotent: a re-run
 * recomputes and renames only what differs, and the reservation stops double renames.
 * @param {{ guildId: string }} job
 * @param {{ guild: object }} ctx
 * @returns {Promise<void|{ nextRunAt: Date }>} void deletes the job (no counters left)
 */
export async function refreshJobHandler(job, ctx) {
  const now = Date.now();
  pendingPull.delete(job.guildId); // later member events may pull again
  try {
    const guild = ctx.guild;
    if (!guild?.available) return { nextRunAt: new Date(now + 5 * 60_000) };
    const rows = await loadRows(job.guildId); // throws on a DB error (caught below)
    if (rows.length === 0) return undefined;
    const settings = await getSettingsSafe(job.guildId);
    if (!settings) return { nextRunAt: new Date(now + 2 * 60_000) };
    if (!isModuleEnabled(settings, 'stats')) return { nextRunAt: new Date(now + REFRESH_INTERVAL_MS) }; // idle while off
    const out = await runExclusive(job.guildId, () => refreshGuild(guild, rows, { allowFetch: true, startedAt: now }));
    if (out.busy) return { nextRunAt: new Date(now + MIN_NEXT_RUN_MS) };
    let next = out.value.nextRunAt;
    // A member event that claimed a pull while this run was in flight could not move the (RUNNING) job, and the snapshot may
    // predate it: cap the next run at the claimed time so the trailing value is never dropped.
    const pulled = pendingPull.get(job.guildId);
    if (pulled !== undefined) {
      const cap = Math.max(pulled, Date.now() + PULL_FLOOR_MS);
      if (cap < next.getTime()) next = new Date(cap);
    }
    return { nextRunAt: next };
  } catch (err) {
    logger.error(`stats.refresh failed for guild ${job.guildId}`, err);
    return { nextRunAt: new Date(Date.now() + 5 * 60_000) };
  }
}

// ── Hook bodies and filters ───────────────────────────────────────────────────

/** guildReady (always; database only): primes the memo and keeps the job alive. */
export async function onGuildReady(ctx) {
  let rows;
  try {
    rows = await loadRows(ctx.guildId);
  } catch (err) {
    logger.error(`stats: could not load the counters of guild ${ctx.guildId} on startup`, err);
    return;
  }
  // Catches up on changes made while the bot was offline, spread out so a restart is not a thundering herd. With no rows the memo is
  // primed to [], so later member events of this guild are filtered out without database work.
  if (rows.length > 0) {
    await ensureRefreshJob(ctx.guildId, new Date(Date.now() + STARTUP_DELAY_MS + Math.floor(Math.random() * STARTUP_JITTER_MS)));
  }
}

/** Sync filter of guildMemberAdd / guildMemberRemove. Degrade rule: no or expired memo fails open (the body reads Postgres). */
export function memberEventFilter(ctx) {
  const m = freshMemo(ctx.guildId);
  if (!m) return true;
  return m.rows.some((r) => MEMBER_EVENT_KINDS.has(r.kind));
}

/** guildMemberAdd / guildMemberRemove (gated by the module). Bots joining count too: MEMBERS and BOTS change. */
export async function onMemberChange(ctx) {
  const rows = await getGuildRows(ctx.guildId); // null on a DB error: fail closed (no pull; the 10-minute job retries)
  if (!rows) return;
  if (!rows.some((r) => MEMBER_EVENT_KINDS.has(r.kind))) return;
  if (!claimPull(ctx.guildId)) return; // a pull is already pending and that run will see this change too
  await pullRefresh(ctx.guildId, MEMBER_PULL_DELAY_MS);
}

/** Sync filter of channelDelete. Degrade rule: no or expired memo fails open for voice channels. */
export function channelDeleteFilter(ctx) {
  const t = ctx.channel?.type;
  if (t === ChannelType.GuildCategory) {
    const s = peekSettings(ctx.guildId);
    return !s || s.statsCategoryId === ctx.channel.id;
  }
  if (t === ChannelType.GuildVoice) {
    const m = freshMemo(ctx.guildId);
    if (!m) return true;
    return m.rows.some((r) => r.channelId === ctx.channel.id);
  }
  return false;
}

/** channelDelete (always: cleanup must happen while the module is off). A delete event is authoritative (rule 20). */
export async function onChannelDelete(ctx) {
  try {
    if (ctx.channel.type === ChannelType.GuildVoice) {
      const { count } = await prisma.statChannel.deleteMany({ where: { guildId: ctx.guildId, channelId: ctx.channel.id } });
      if (count > 0) {
        const m = rowsMemo.get(ctx.guildId);
        if (m) m.rows = m.rows.filter((r) => r.channelId !== ctx.channel.id);
        logger.info(`Stat channel ${ctx.channel.id} deleted in ${ctx.guildId}; counter removed`);
        if (await prisma.statChannel.count({ where: { guildId: ctx.guildId } }) === 0) await cancelRefreshJob(ctx.guildId);
      }
    } else if (ctx.channel.type === ChannelType.GuildCategory) {
      // Discord moves the child channels to the top level; it does not delete them.
      const s = await ctx.getSettings();
      if (s?.statsCategoryId === ctx.channel.id) await updateSettings(ctx.guildId, { statsCategoryId: null });
    }
  } catch (err) {
    logger.error(`stats: channelDelete cleanup failed in guild ${ctx.guildId}`, err);
  }
}

// ── Service functions used by /stats ──────────────────────────────────────────

/**
 * Creates the locked voice channel and its row. Throws a StatsError (LIMIT, ROLE_LIMIT, DUPLICATE, MISSING_PERMISSIONS,
 * CHANNEL_LIMIT, DISCORD_REJECTED) or the original DB error.
 * @returns {Promise<{ channel: object, row: object, value: number|null }>}
 */
export async function createStatChannel({
  guild, client, kind, template, role = null, category = null, actorTag,
}) {
  const made = await configMutex.run(guild.id, async () => {
    // 1. Load the rows and prune the ones whose channel Discord confirms is gone (rule 20: a cache miss alone never prunes).
    let rows = await listRows(guild.id);
    const goneIds = [];
    for (const r of rows) {
      if (guild.channels.cache.has(r.channelId)) continue;
      if ((await confirmChannel(guild, r.channelId)).state === 'gone') goneIds.push(r.id);
    }
    if (goneIds.length) {
      await prisma.statChannel.deleteMany({ where: { id: { in: goneIds } } });
      rows = rows.filter((r) => !goneIds.includes(r.id));
      primeMemo(guild.id, rows);
    }

    // 2. Caps (rows whose channel could not be checked still count).
    if (rows.length >= LIMITS.STAT_CHANNELS) throw new StatsError('LIMIT');
    if (kind === 'ROLE_MEMBERS' && rows.filter((r) => r.kind === 'ROLE_MEMBERS').length >= LIMITS.ROLE_STAT_CHANNELS) {
      throw new StatsError('ROLE_LIMIT');
    }

    // 3. One counter per kind (per role for ROLE_MEMBERS).
    const dup = rows.find((r) => r.kind === kind && (kind !== 'ROLE_MEMBERS' || r.roleId === role?.id));
    if (dup) throw new StatsError('DUPLICATE', { channelId: dup.channelId });

    // 4. Initial value (may be null: the name shows an ellipsis until the next refresh).
    const { snap } = await collectSnapshot(guild, [{ kind, roleId: role?.id ?? null }], { allowFetch: true });
    const { value } = computeValue(kind, snap, { roleId: role?.id ?? null, peakValue: null });
    const name = renderStatName(template, { count: value, roleName: role?.name ?? null, guild });

    // 5-6. The settings row first (FK), then the channel.
    await ensureGuildSettings(guild.id);
    let channel;
    try {
      channel = await guild.channels.create({
        name,
        type: ChannelType.GuildVoice,
        parent: category?.id ?? null,
        permissionOverwrites: buildOverwrites(guild, client.user.id),
        reason: clampText(`Statistics channel created by ${actorTag}`, 512),
      });
    } catch (err) {
      const code = discordErrorCode(err);
      if (code === 50013 || code === 50001) throw new StatsError('MISSING_PERMISSIONS');
      if (code === 30013) throw new StatsError('CHANNEL_LIMIT');
      if (code === 50035) throw new StatsError('DISCORD_REJECTED');
      throw err;
    }

    // 7. Save the row; on failure remove the channel again so nothing is orphaned.
    let row;
    try {
      row = await prisma.statChannel.create({
        data: {
          guildId: guild.id,
          channelId: channel.id,
          kind,
          roleId: role?.id ?? null,
          template,
          lastValue: value,
          lastRenamedAt: null,
          peakValue: kind === 'PEAK_ONLINE' ? value : null,
        },
      });
    } catch (err) {
      markBotDeletion(channel.id, 'stats');
      await channel.delete('Statistics channel could not be saved').catch(() => {});
      throw err;
    }
    memoAddRow(guild.id, row);
    return { channel, row, value };
  });

  // 9. After the mutex.
  await ensureRefreshJob(guild.id, new Date(Date.now() + (made.value == null ? MIN_NEXT_RUN_MS : REFRESH_INTERVAL_MS)));
  return made;
}

/** Changes a counter's template; the rename follows through a pull of the job. Throws StatsError('NOT_FOUND') when the row vanished. */
export async function editTemplate(guild, row, template) {
  await configMutex.run(guild.id, async () => {
    const { count } = await prisma.statChannel.updateMany({ where: { id: row.id, guildId: guild.id }, data: { template } });
    if (count === 0) throw new StatsError('NOT_FOUND');
    memoAddRow(guild.id, row);
  });
  await pullRefresh(guild.id, EDIT_PULL_DELAY_MS);
}

/**
 * Removes a counter and (unless keepChannel) its Discord channel. Throws StatsError('MISSING_PERMISSIONS') and changes nothing
 * when the bot cannot delete the channel.
 * @returns {Promise<{ channelDeleted: boolean }>}
 */
export async function deleteStatChannel(guild, row, { keepChannel = false, actorTag } = {}) {
  return configMutex.run(guild.id, async () => {
    let channelDeleted = false;
    if (!keepChannel) {
      const reason = clampText(`Statistics channel removed by ${actorTag}`, 512);
      const cached = guild.channels.cache.get(row.channelId);
      if (cached && missingBotPermissions(cached, ['ViewChannel', 'ManageChannels']).length) throw new StatsError('MISSING_PERMISSIONS');
      markBotDeletion(row.channelId, 'stats'); // logging labels the delete "by the bot"
      try {
        // A channel missing from the cache may still exist, so it is deleted by id and a 10003 counts as success (rule 20).
        if (cached) await cached.delete(reason);
        else await guild.channels.delete(row.channelId, reason);
        channelDeleted = true;
      } catch (err) {
        const code = discordErrorCode(err);
        if (code === 10003) channelDeleted = false;
        else if (code === 50013 || code === 50001) throw new StatsError('MISSING_PERMISSIONS');
        else throw err;
      }
    }
    await prisma.statChannel.deleteMany({ where: { id: row.id, guildId: guild.id } }); // idempotent with the channelDelete hook
    forgetRow(guild.id, row.id);
    if (await prisma.statChannel.count({ where: { guildId: guild.id } }) === 0) await cancelRefreshJob(guild.id);
    return { channelDeleted };
  });
}

/**
 * Refreshes every counter now (still bound by the per-channel rename gap).
 * @returns {Promise<{ empty: true } | { busy: true } | { result: object }>}
 */
export async function refreshNow(guild) {
  const rows = await listRows(guild.id);
  if (rows.length === 0) return { empty: true };
  const out = await runExclusive(guild.id, () => refreshGuild(guild, rows, { allowFetch: true }));
  if (out.busy) return { busy: true };
  await ensureRefreshJob(guild.id, out.value.nextRunAt);
  return { result: out.value };
}
