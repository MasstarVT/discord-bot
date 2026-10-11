// Invite Tracker service (pkg-invites). Everything that decides WHO invited a member lives here;
// src/plugins/invites.js and src/commands/utility/invites.js are thin wrappers.
//
// Model in one paragraph: a per-guild Snapshot (every invite's use count, a 2-minute tombstone for invites
// that vanished, and the vanity-URL baseline) is kept in memory and mirrored to Redis with a 7-day TTL. A
// join takes a fresh observation (one invites fetch + one vanity fetch) and diffs it against the snapshot
// INSIDE a per-guild mutex. Exactly one candidate invite means a match; zero or several mean "unknown".
// Nothing is ever guessed: a missing, legacy or pre-restart snapshot only re-snapshots (R139/R141/R142).
import { PermissionFlagsBits, RESTJSONErrorCodes } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings } from './settingsService.js';
import { createKeyedMutex, createThrottle, withTimeout } from '../utils/async.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const SNAPSHOT_VERSION = 1;
export const SNAPSHOT_TTL_SECONDS = 7 * 24 * 60 * 60;   // 604800, re-set on every write
export const TOMBSTONE_TTL_MS = 2 * 60_000;              // tombstones match and live this long
export const FAKE_THRESHOLD_MS = 24 * 60 * 60_000;       // MEE6 24 h validity rule
export const MAX_PENDING_JOINS = 5;                      // waiters per guild before a join is treated as a burst
export const FETCH_TIMEOUT_MS = 5_000;                   // per invites / vanity fetch
export const JOIN_HOOK_WAIT_MS = 7_000;                  // the plugin stops waiting; tracking continues in the background
export const MEMO_MAX_GUILDS = 10_000;
export const LINK_COOLDOWN_SECONDS = 60;
export const snapshotKey = (guildId) => `guild:${guildId}:invites`;

// ── Pure helpers (no I/O; exported for tests) ─────────────────────────────────

const isPlainObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const own = (obj, key) => (obj && Object.hasOwn(obj, key) ? obj[key] : undefined);
const toInt = (n) => Math.max(0, Math.trunc(Number(n) || 0));
const toMs = (d) => (d instanceof Date ? d.getTime() : Number(d));

/** The snapshot shape check. The legacy value `{ [code]: uses }` fails it and counts as missing. */
export function isSnapshot(x) {
  if (!isPlainObject(x) || x.v !== SNAPSHOT_VERSION) return false;
  if (!Number.isFinite(x.takenAt)) return false;
  if (!isPlainObject(x.codes) || !isPlainObject(x.tombstones)) return false;
  if (x.vanity === null) return true;
  return isPlainObject(x.vanity) && typeof x.vanity.code === 'string' && Number.isInteger(x.vanity.uses);
}

/** A snapshot is trusted only when it was observed by THIS process (after the shard became ready). */
export function isTrustedSnapshot(snap, trustedSince) {
  return isSnapshot(snap) && snap.takenAt >= trustedSince;
}

/** Normalises one stored or observed code entry (read defensively: entries are not deep-validated). */
const normState = (e) => ({
  uses: toInt(e?.uses),
  maxUses: toInt(e?.maxUses),
  inviterId: e?.inviterId ?? null,
  expiresAt: e?.expiresAt ?? null,
});

/** @param {import('discord.js').Invite|object} invite */
export function inviteToState(invite) {
  return {
    code: invite.code,
    uses: invite.uses ?? 0,
    maxUses: invite.maxUses ?? 0,
    inviterId: invite.inviterId ?? invite.inviter?.id ?? null,
    expiresAt: invite.expiresTimestamp ?? null,
  };
}

export function emptySnapshot(now) {
  return { v: SNAPSHOT_VERSION, takenAt: now, codes: {}, tombstones: {}, vanity: null };
}

const tombstoneAlive = (t, now) => now - Number(t?.deletedAt) <= TOMBSTONE_TTL_MS;

/**
 * Builds a fresh snapshot from a full observation. `previous` must be TRUSTED (or null): its tombstones
 * and its vanished codes carry over; an unknown vanity baseline is never kept (R140/R145).
 * @returns {object} Snapshot (inputs are never mutated)
 */
export function buildSnapshot(observation, { previous = null, now = Date.now() } = {}) {
  const codes = {};
  for (const o of observation?.invites ?? []) codes[o.code] = normState(o);

  const tombstones = {};
  if (previous) {
    for (const [c, t] of Object.entries(previous.tombstones ?? {})) {
      if (Object.hasOwn(codes, c) || !tombstoneAlive(t, now)) continue;
      tombstones[c] = { ...normState(t), deletedAt: Number(t.deletedAt) };
    }
    for (const [c, s] of Object.entries(previous.codes ?? {})) {
      if (Object.hasOwn(codes, c) || Object.hasOwn(tombstones, c)) continue;
      tombstones[c] = { ...normState(s), deletedAt: now };   // vanished since the last observation
    }
  }

  const v = observation?.vanity;
  const vanity = v && typeof v === 'object' ? { code: String(v.code), uses: toInt(v.uses) } : null;
  return { v: SNAPSHOT_VERSION, takenAt: now, codes, tombstones, vanity };
}

function pruned(tombstones, now) {
  const out = {};
  for (const [c, t] of Object.entries(tombstones ?? {})) if (tombstoneAlive(t, now)) out[c] = { ...t };
  return out;
}

/** invite_create: adds the code unless it is already known (the known entry may have higher uses). takenAt is unchanged. */
export function applyInviteCreate(snapshot, inviteState, { now = Date.now() } = {}) {
  const codes = {};
  for (const [c, s] of Object.entries(snapshot.codes ?? {})) codes[c] = { ...s };
  if (!Object.hasOwn(codes, inviteState.code)) codes[inviteState.code] = normState(inviteState);
  return {
    v: SNAPSHOT_VERSION,
    takenAt: snapshot.takenAt,
    codes,
    tombstones: pruned(snapshot.tombstones, now),
    vanity: snapshot.vanity ? { ...snapshot.vanity } : null,
  };
}

/**
 * invite_delete: the code moves to a tombstone so its last use count survives for a join that is still
 * queued (R143/R144). Never refetches. takenAt is unchanged.
 */
export function applyInviteDelete(snapshot, code, { now = Date.now() } = {}) {
  const codes = {};
  for (const [c, s] of Object.entries(snapshot.codes ?? {})) codes[c] = { ...s };
  const tombstones = pruned(snapshot.tombstones, now);
  if (Object.hasOwn(codes, code)) {
    tombstones[code] = { ...normState(codes[code]), deletedAt: now };
    delete codes[code];
  }
  return {
    v: SNAPSHOT_VERSION,
    takenAt: snapshot.takenAt,
    codes,
    tombstones,
    vanity: snapshot.vanity ? { ...snapshot.vanity } : null,
  };
}

/**
 * Which invite did a join use? `before` must be a trusted snapshot. Never takes "the first one":
 * exactly one candidate matches; none gives 'none', two or more give 'ambiguous' (R276).
 * @returns {{ match: object|null, reason: 'matched'|'none'|'ambiguous', candidates: object[], next: object }}
 */
export function resolveJoin(before, observation, { now = Date.now() } = {}) {
  const invites = observation?.invites ?? [];
  const observed = new Set(invites.map((o) => o.code));
  const candidates = [];

  // 1. Live increments. A code absent from `before` was created after the last full observation (baseline 0).
  for (const o of invites) {
    const prev = own(before.codes, o.code) ?? own(before.tombstones, o.code);
    const delta = toInt(o.uses) - toInt(prev?.uses);
    if (delta > 0) candidates.push({ code: o.code, inviterId: o.inviterId ?? prev?.inviterId ?? null, delta, kind: 'live' });
  }

  // 2. Vanished, consumable codes (R143/R144): one use left before exhaustion, not expired, tombstone fresh.
  const vanished = new Map();
  for (const [c, s] of Object.entries(before.codes ?? {})) if (!observed.has(c)) vanished.set(c, { state: s, tombstone: false });
  for (const [c, t] of Object.entries(before.tombstones ?? {})) if (!observed.has(c)) vanished.set(c, { state: t, tombstone: true });
  for (const [c, { state, tombstone }] of vanished) {
    const { uses, maxUses, inviterId, expiresAt } = normState(state);
    if (!(maxUses > 0 && uses === maxUses - 1)) continue;
    if (!(expiresAt == null || expiresAt > now)) continue;
    if (tombstone && !tombstoneAlive(state, now)) continue;
    candidates.push({ code: c, inviterId, delta: 1, kind: 'vanished' });
  }

  // 3. Vanity URL (R140/R145): only against a KNOWN baseline for the same code.
  const ov = observation?.vanity;
  const bv = before.vanity;
  if (ov && typeof ov === 'object' && bv && typeof bv === 'object' && ov.code === bv.code) {
    const delta = toInt(ov.uses) - toInt(bv.uses);
    if (delta > 0) candidates.push({ code: ov.code, inviterId: null, delta, kind: 'vanity' });
  }

  // 4. Decide.
  let match = null;
  let reason = 'none';
  if (candidates.length === 1) { match = candidates[0]; reason = 'matched'; }
  else if (candidates.length > 1) reason = 'ambiguous';

  // 5. Next snapshot. Every vanished candidate (matched or ambiguous) is marked exhausted so a consumed
  //    single-use code can never match a second join.
  const next = buildSnapshot(observation, { previous: before, now });
  for (const c of candidates) {
    if (c.kind !== 'vanished') continue;
    const src = vanished.get(c.code);
    const st = normState(src.state);
    const base = next.tombstones[c.code] ?? { ...st, deletedAt: src.tombstone ? Number(src.state.deletedAt) : now };
    next.tombstones[c.code] = { ...base, uses: st.maxUses };
  }
  return { match, reason, candidates, next };
}

/** One tracked join (a GuildMemberCache row with an inviter) -> 'active' | 'left' | 'fake' (see the 24 h rule). */
export function classifyJoin(row) {
  if (row.inviterId != null && row.inviterId === row.userId) return 'fake';
  if (row.leftAt == null) return 'active';
  return toMs(row.leftAt) - toMs(row.joinedAt) >= FAKE_THRESHOLD_MS ? 'left' : 'fake';
}

/** The executable definition the SQL in RAW_SQL must match. Only rows with an inviter count. */
export function computeStats(rows, { now = Date.now() } = {}) {
  const s = { total: 0, active: 0, left: 0, fake: 0, valid: 0, recent: 0 };
  for (const row of rows ?? []) {
    if (row.inviterId == null) continue;
    const cls = classifyJoin(row);
    s[cls] += 1;
    if (cls === 'active' && now - toMs(row.joinedAt) < FAKE_THRESHOLD_MS) s.recent += 1;
  }
  s.valid = s.active + s.left;
  s.total = s.active + s.left + s.fake;
  return s;
}

// ── Module-scope state (in-memory only; no timers or I/O at import) ───────────

/** @type {Map<string, object>} guildId -> Snapshot, capped at MEMO_MAX_GUILDS */
const memo = new Map();
/** guildId -> joins enqueued but not started */
const pending = new Map();
const guildMutex = createKeyedMutex();
const permWarn = createThrottle(6 * 60 * 60_000);
const dbWarn = createThrottle(60_000);

// ── Snapshot store (used only inside the guild mutex) ─────────────────────────
// Redis decision: FAIL-CLOSED for attribution. A cache.get that returns null (Redis down or key missing)
// with an empty memo means "no snapshot", so the join is recorded as unknown and a fresh snapshot is taken.
// Writes always land in the memo, so a Redis outage on a running shard keeps attribution working.

async function loadSnapshot(guildId) {
  const hit = memo.get(guildId);
  if (hit) return hit;
  const raw = await cache.get(snapshotKey(guildId));
  if (!isSnapshot(raw)) return null;
  memoSet(guildId, raw);
  return raw;
}

function memoSet(guildId, snap) {
  memo.delete(guildId);
  memo.set(guildId, snap);
  while (memo.size > MEMO_MAX_GUILDS) memo.delete(memo.keys().next().value);
}

async function saveSnapshot(guildId, snap) {
  memoSet(guildId, snap);
  await cache.set(snapshotKey(guildId), snap, SNAPSHOT_TTL_SECONDS); // a false result is fine: the memo holds it
}

async function dropSnapshot(guildId) {
  memo.delete(guildId);
  await cache.del(snapshotKey(guildId));
}

const trustedSince = (guild) => guild.client?.readyTimestamp ?? Number.POSITIVE_INFINITY;

/** Manage Server is required for GET /guilds/{id}/invites and /vanity-url; nothing else is needed for tracking. */
export function botCanTrack(guild) {
  const me = guild?.members?.me;
  return !me || me.permissions.has(PermissionFlagsBits.ManageGuild);
}

function warnNoPermission(guild) {
  if (permWarn(guild.id)) logger.warn(`Invite tracking unavailable in ${guild.id}: missing Manage Server`);
}

/**
 * One full observation: the invite list plus the vanity data (when the guild has a vanity URL).
 * Returns null when the invite list could not be fetched.
 * @returns {Promise<{ invites: object[], vanity: { code: string, uses: number }|null|undefined }|null>}
 */
async function observe(guild) {
  const [inv, van] = await Promise.allSettled([
    withTimeout((async () => guild.invites.fetch({ cache: false }))(), FETCH_TIMEOUT_MS, 'invites fetch'),
    guild.vanityURLCode
      ? withTimeout((async () => guild.fetchVanityData())(), FETCH_TIMEOUT_MS, 'vanity fetch')
      : Promise.resolve(null),
  ]);
  if (inv.status === 'rejected') {
    const code = inv.reason?.code;
    if (code === RESTJSONErrorCodes.MissingPermissions || code === RESTJSONErrorCodes.MissingAccess) warnNoPermission(guild);
    else logger.debug(`Invite fetch failed in ${guild.id}: ${inv.reason?.message ?? inv.reason}`);
    return null;
  }
  return {
    invites: [...inv.value.values()].map(inviteToState),
    // object = fetched; null = guild has no vanity; undefined = fetch failed (baseline unknown)
    vanity: van.status === 'fulfilled'
      ? (van.value ? { code: van.value.code, uses: van.value.uses ?? 0 } : null)
      : undefined,
  };
}

// ── Runtime ───────────────────────────────────────────────────────────────────

/**
 * Warms (or re-baselines) the snapshot: called by the guildReady hook. Never throws.
 * A stored snapshot from a previous process is untrusted and contributes nothing.
 * @returns {Promise<{ ok: boolean, reason?: string }>}
 */
export async function refreshSnapshot(guild) {
  try {
    return await guildMutex.run(guild.id, async () => {
      if (!guild.available) return { ok: false, reason: 'unavailable' };
      if (!botCanTrack(guild)) { warnNoPermission(guild); return { ok: false, reason: 'no-permission' }; }
      const observation = await observe(guild);
      if (!observation) return { ok: false, reason: 'fetch-failed' };   // the stored snapshot is left untouched
      const prev = await loadSnapshot(guild.id);
      const previous = isTrustedSnapshot(prev, trustedSince(guild)) ? prev : null;
      await saveSnapshot(guild.id, buildSnapshot(observation, { previous, now: Date.now() }));
      return { ok: true };
    });
  } catch (err) {
    logger.warn(`Invite snapshot refresh failed in ${guild?.id}: ${err?.message ?? err}`);
    return { ok: false, reason: 'error' };
  }
}

/** inviteCreate: incremental edit, no REST call, nothing without a snapshot (never a partial one). Never throws. */
export async function recordInviteCreate(guild, invite) {
  try {
    await guildMutex.run(guild.id, async () => {
      const snap = await loadSnapshot(guild.id);
      if (!snap) return;
      await saveSnapshot(guild.id, applyInviteCreate(snap, inviteToState(invite), { now: Date.now() }));
    });
  } catch (err) {
    logger.warn(`Invite create record failed in ${guild?.id}: ${err?.message ?? err}`);
  }
}

/** inviteDelete: the code becomes a tombstone (R143). Never refetches: a refetch would absorb a queued join's use. Never throws. */
export async function recordInviteDelete(guild, invite) {
  try {
    await guildMutex.run(guild.id, async () => {
      const snap = await loadSnapshot(guild.id);
      if (!snap) return;
      await saveSnapshot(guild.id, applyInviteDelete(snap, invite.code, { now: Date.now() }));
    });
  } catch (err) {
    logger.warn(`Invite delete record failed in ${guild?.id}: ${err?.message ?? err}`);
  }
}

/** Attribution for one join; runs inside the guild mutex. @returns {Promise<{ match: object|null, reason: string }>} */
async function attribute(guild) {
  const gid = guild.id;
  if (!guild.available) return { match: null, reason: 'unavailable' };
  const before = await loadSnapshot(gid);
  const observation = await observe(guild);
  if (!observation) return { match: null, reason: 'fetch-failed' };      // snapshot untouched

  const now = Date.now();
  if (!isTrustedSnapshot(before, trustedSince(guild))) {
    // Missing, legacy or left over from a previous process: never diff (R139/R141/R142), just re-baseline.
    await saveSnapshot(gid, buildSnapshot(observation, { previous: null, now }));
    return { match: null, reason: 'no-snapshot' };
  }

  const r = resolveJoin(before, observation, { now });
  await saveSnapshot(gid, r.next);                                       // vanity baseline included (R140/R145)
  if (!r.match || r.match.kind === 'vanity') return { match: r.match, reason: r.reason };

  let inviterId = r.match.inviterId;
  try {
    const p = await prisma.personalInvite.findUnique({ where: { code: r.match.code } });
    if (p && p.guildId === gid) inviterId = p.userId;                    // /invites link codes are created by the bot
  } catch { /* keep the snapshot inviter */ }
  if (inviterId === guild.client?.user?.id) inviterId = null;            // never credit the bot itself
  return { match: { ...r.match, inviterId }, reason: r.reason };
}

/**
 * Records one join (always) and attributes it when possible. The synchronous prologue enqueues on the
 * guild mutex before any await, so the queue order equals the gateway order (a join followed by a leave
 * is processed join first). Never rejects.
 * @param {import('discord.js').GuildMember} member
 * @param {{ isEnabled?: () => Promise<boolean> }} [opts]
 * @returns {Promise<{ code: string, inviterId: string|null, vanity: boolean }|null>} null = unknown
 */
export function trackJoin(member, { isEnabled = async () => true } = {}) {
  const guild = member.guild;
  const gid = guild.id;
  const waiting = pending.get(gid) ?? 0;
  const burst = waiting >= MAX_PENDING_JOINS;
  pending.set(gid, waiting + 1);
  const joinedAt = member.joinedAt ?? new Date();
  return guildMutex.run(gid, async () => {
    const left = (pending.get(gid) ?? 1) - 1;
    if (left > 0) pending.set(gid, left); else pending.delete(gid);

    try {
      let match = null;
      let reason;
      if (!(await isEnabled())) {
        await dropSnapshot(gid);                // the snapshot drifts while the module is off: re-baseline on the next join
        reason = 'disabled';
      } else if (burst) {
        await dropSnapshot(gid);                // a raid: never queue more than MAX_PENDING_JOINS REST diffs per guild
        reason = 'burst';
      } else if (!botCanTrack(guild)) {
        warnNoPermission(guild);
        reason = 'no-permission';
      } else {
        ({ match, reason } = await attribute(guild));
      }

      // R185/R292: the row is written for EVERY join, with a null inviter when unknown (latest join wins).
      try {
        await ensureGuildSettings(gid);
        const data = { joinedAt, inviterId: match?.inviterId ?? null, inviteCode: match?.code ?? null, leftAt: null };
        await prisma.guildMemberCache.upsert({
          where: { guildId_userId: { guildId: gid, userId: member.id } },
          create: { guildId: gid, userId: member.id, ...data },
          update: data,
        });
      } catch (err) {
        if (dbWarn(gid)) logger.warn(`Invite join record failed in ${gid}: ${err?.message ?? err}`);
      }

      logger.debug(`Invite attribution ${gid}/${member.id}: ${reason}${match ? ` ${match.code}` : ''}`);
      return match ? { code: match.code, inviterId: match.inviterId, vanity: match.kind === 'vanity' } : null;
    } catch (err) {
      logger.error(`Invite attribution failed in ${gid}`, err instanceof Error ? err : new Error(String(err)));
      return null;
    }
  }).catch(() => null);
}

/**
 * Sets leftAt on the member's tracked join. Shares the join queue, so a join followed immediately by a
 * leave is processed in that order. Never creates a row and never rejects.
 * @param {import('discord.js').Guild} guild
 * @param {string} userId
 */
export function trackLeave(guild, userId) {
  if (!guild?.id || !userId) return Promise.resolve();
  const leftAt = new Date();
  return guildMutex.run(guild.id, async () => {
    try {
      await prisma.guildMemberCache.updateMany({ where: { guildId: guild.id, userId, leftAt: null }, data: { leftAt } });
    } catch (err) {
      if (dbWarn(guild.id)) logger.warn(`Invite leave record failed in ${guild.id}: ${err?.message ?? err}`);
    }
  }).catch(() => {});
}

/**
 * Is a code still alive? Reads only (outside the mutex; a stale read is fine). Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {string} code
 * @param {{ allowFetch?: boolean, since?: number|null }} [opts] since = when the code was created (epoch ms)
 * @returns {Promise<'live'|'gone'|'unknown'>}
 */
export async function getCodeState(guild, code, { allowFetch = true, since = null } = {}) {
  try {
    const snap = await loadSnapshot(guild.id);
    if (own(snap?.codes, code)) return 'live';
    if (own(snap?.tombstones, code)) return 'gone';
    // A full observation taken after the link was created did not contain it.
    if (snap && since != null && snap.takenAt > since) return 'gone';
    if (!allowFetch) return 'unknown';
    try {
      const inv = await withTimeout(guild.client.fetchInvite(code), FETCH_TIMEOUT_MS, 'fetchInvite');
      return inv?.guild?.id === guild.id ? 'live' : 'gone';
    } catch (err) {
      return err?.code === RESTJSONErrorCodes.UnknownInvite ? 'gone' : 'unknown';
    }
  } catch {
    return 'unknown';
  }
}

/** Best-effort invite deletion (needs Manage Channels on the channel or Manage Server). Never throws. */
export async function deleteDiscordInvite(guild, code, reason) {
  try {
    await guild.invites.delete(code, reason);
  } catch (err) {
    logger.debug(`Could not delete invite ${code} in ${guild?.id}: ${err?.message ?? err}`);
  }
}

// ── Stats SQL (must match computeStats) ───────────────────────────────────────
// Every COUNT is cast ::int (a raw COUNT is a BigInt). "left" is a reserved word, so the column is "leftCount".

const FILTERS = Prisma.sql`
  COUNT(*)::int AS "total",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NULL)::int AS "active",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NOT NULL
                     AND "leftAt" - "joinedAt" >= interval '24 hours')::int AS "leftCount",
  COUNT(*) FILTER (WHERE "userId" = "inviterId"
                     OR ("leftAt" IS NOT NULL AND "leftAt" - "joinedAt" < interval '24 hours'))::int AS "fake",
  COUNT(*) FILTER (WHERE "userId" <> "inviterId" AND "leftAt" IS NULL
                     AND "joinedAt" > ${SQL_NOW} - interval '24 hours')::int AS "recent"`;
const VALID = Prisma.sql`COUNT(*) FILTER (WHERE "userId" <> "inviterId"
                     AND ("leftAt" IS NULL OR "leftAt" - "joinedAt" >= interval '24 hours'))`;

const statsQuery = (guildId, userId) => Prisma.sql`
  SELECT ${FILTERS} FROM "GuildMemberCache" WHERE "guildId" = ${guildId} AND "inviterId" = ${userId}`;

const leaderboardQuery = (guildId, limit, excludeIds) => Prisma.sql`
  SELECT "inviterId", ${FILTERS}, ${VALID}::int AS "valid"
  FROM "GuildMemberCache"
  WHERE "guildId" = ${guildId} AND "inviterId" IS NOT NULL AND NOT ("inviterId" = ANY(${excludeIds}::text[]))
  GROUP BY "inviterId"
  HAVING ${VALID} > 0
  ORDER BY "valid" DESC, "inviterId" ASC
  LIMIT ${Math.min(Math.max(Math.trunc(Number(limit)) || 1, 1), 200)}`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'invites.stats', build: () => statsQuery('0', '0') },
  { name: 'invites.leaderboard', build: () => leaderboardQuery('0', 10, ['0']) },
]);

const mapStats = (r) => {
  const active = Number(r?.active ?? 0);
  const left = Number(r?.leftCount ?? 0);
  return { total: Number(r?.total ?? 0), active, left, fake: Number(r?.fake ?? 0), valid: active + left, recent: Number(r?.recent ?? 0) };
};

// ── DB reads and writes for the command (THROW on DB failure; the command catches) ──

/** @returns {Promise<{ total: number, active: number, left: number, fake: number, valid: number, recent: number }>} */
export async function getInviteStats(guildId, userId) {
  const rows = await prisma.$queryRaw(statsQuery(guildId, userId));
  return mapStats(rows[0]);
}

/** Top inviters by valid invites (rows with valid = 0 are left out). */
export async function getInviteLeaderboard(guildId, { limit = 200, excludeIds = [] } = {}) {
  const rows = await prisma.$queryRaw(leaderboardQuery(guildId, limit, excludeIds.map(String)));
  return rows.map((r) => {
    const stats = mapStats(r);
    if (Number(r.valid) !== stats.valid) logger.warn(`Invite leaderboard: valid mismatch for ${r.inviterId} (${r.valid} vs ${stats.valid})`);
    return { inviterId: r.inviterId, ...stats };
  });
}

/** @returns {Promise<object|null>} the GuildMemberCache row (who invited this member, when they joined/left) */
export async function getJoinRecord(guildId, userId) {
  return prisma.guildMemberCache.findUnique({ where: { guildId_userId: { guildId, userId } } });
}

/** @returns {Promise<object|null>} */
export async function getPersonalInvite(guildId, userId) {
  return prisma.personalInvite.findUnique({ where: { guildId_userId: { guildId, userId } } });
}

/** Upserts the member's personal link. The update branch resets createdAt (getCodeState compares against it). */
export async function savePersonalInvite({ guildId, userId, code, channelId }) {
  await ensureGuildSettings(guildId);
  return prisma.personalInvite.upsert({
    where: { guildId_userId: { guildId, userId } },
    create: { guildId, userId, code, channelId },
    update: { code, channelId, createdAt: new Date() },
  });
}
