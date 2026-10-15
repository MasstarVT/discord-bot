// Birthdays (pkg-birthdays): pure date helpers, all database access for `Birthday`, the hourly `birthday.tick` job (announce at the
// guild-local hour, grant and remove the birthday role), the role transfer and the plugin hook. Commands only read options and format
// replies; this file owns the behaviour (pkg-birthdays.md §6).
//
// Side-effect free at import: module-level Set / mutex / throttle objects only, no timers, no I/O.
//
// Redis (cache only, never the source of truth): `birthday:announced:{guildId}:{userId}` is a tombstone holding the guild-local year of
// the last completed celebration. FAIL-OPEN: a miss or an unreachable Redis means "no carry-over", so the worst case is one extra
// celebration for a member who forgot and re-added their birthday in the same year while Redis was down.
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import { cache } from './redis.js';
import { scheduleJob, cancelJobs, getJob } from './scheduler.js';
import { ensureGuildSettings, getSettings, isModuleEnabled } from './settingsService.js';
import { createKeyedMutex, createThrottle } from '../utils/async.js';
import { checkAssignableRole, checkChannelUsable } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { clampText } from '../utils/text.js';
import {
  daysInMonth, formatTimeOfDay, isValidTimeZone, localDateKey, parseBirthday, zonedParts, zonedToUtc,
} from '../utils/time.js';

// ── Constants (spec §6.0) ─────────────────────────────────────────────────────

export const DEFAULT_BIRTHDAY_MESSAGE = 'Happy birthday {user}! 🎂';
export const DEFAULT_BIRTHDAY_MESSAGE_AGE = 'Happy birthday {user}! They are now {age}! 🎂';
export const TICK_JOB_TYPE = 'birthday.tick';
export const tickKey = (guildId) => `birthday.tick:${guildId}`;
export const tombstoneKey = (guildId, userId) => `birthday:announced:${guildId}:${userId}`;
export const DEFAULT_HOUR = 9;
export const MAX_EVENTS_PER_TICK = 50; // celebrations claimed per run
export const MAX_MARKERS_PER_TICK = 500;
export const MAX_USERS_PER_MESSAGE = 50; // allowed_mentions.users hard cap is 100
export const MAX_MESSAGE_CHARS = 2000;
export const TICK_DEADLINE_MS = 40_000; // stop starting Discord operations; the scheduler's JOB_TIMEOUT_MS is 60 s
export const CONTINUE_MS = 60_000; // continuation run when work remains
export const ERROR_RETRY_MS = 5 * 60_000;
export const SOON_MS = 5_000; // config changes
export const BOUNDARY_SLACK_MS = 2_000; // run just after the local hour boundary
export const MARKER_GIVE_UP_MS = 7 * 24 * 60 * 60_000;
export const NEXT_LIMIT = 10;
export const NEXT_CANDIDATES = 30;
export const MEMBER_FETCH_TIMEOUT_MS = 10_000;
export const MEMBER_FETCH_CHUNK = 100; // gateway request_guild_members accepts at most 100 user ids
export const MONTH_NAMES = Object.freeze([
  'January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December',
]);

const HOUR_MS = 3_600_000;
const ANNOUNCE_NEEDS = Object.freeze(['ViewChannel', 'SendMessages']);

// ── In-process state (lost on restart, harmless) ──────────────────────────────

/** Guild ids whose tick is executing on this shard. */
const running = new Set();
/** Serialises the tick body with transferBirthdayRole, per guild. */
const guildMutex = createKeyedMutex();
/** At most one warning per guild and reason every 6 hours. */
const warnThrottle = createThrottle(6 * HOUR_MS);

function warnOnce(guildId, reason, message) {
  if (warnThrottle(`${guildId}:${reason}`)) logger.warn(message);
}

// ── Pure helpers (spec §6.1) ──────────────────────────────────────────────────

const pad2 = (n) => String(n).padStart(2, '0');
const toDate = (d) => (d instanceof Date ? d : new Date(d));

/** @returns {number} an integer 0-23, or the default (9) for anything else */
export function clampHour(h) {
  return Number.isInteger(h) && h >= 0 && h <= 23 ? h : DEFAULT_HOUR;
}

/** @returns {string} the guild's IANA zone, or 'UTC' when it is empty or invalid */
export function resolveTimeZone(settings) {
  return isValidTimeZone(settings?.timezone) ? settings.timezone : 'UTC';
}

/** @returns {string} 'HH:00' */
export function formatHour(h) {
  return formatTimeOfDay({ hour: clampHour(h), minute: 0 });
}

export function isLeapYear(y) {
  return daysInMonth(y, 2) === 29;
}

/** Feb 29 is celebrated on Feb 28 in non-leap years. */
export function effectiveDay(month, day, year) {
  return month === 2 && day === 29 && !isLeapYear(year) ? 28 : day;
}

/** @param {{ month: number, day: number }} row @param {{ year: number, month: number, day: number }} local */
export function isBirthdayOn(row, local) {
  return row.month === local.month && effectiveDay(row.month, row.day, local.year) === local.day;
}

/** The (month, day) pairs that are a birthday on the local date: Feb 28 also matches Feb 29 rows in non-leap years. */
export function todayMatchers(local) {
  const out = [{ month: local.month, day: local.day }];
  if (!isLeapYear(local.year) && local.month === 2 && local.day === 28) out.push({ month: 2, day: 29 });
  return out;
}

/** The next date (today included) on which the row is celebrated. */
export function nextOccurrence(row, local) {
  const d = effectiveDay(row.month, row.day, local.year);
  if (row.month > local.month || (row.month === local.month && d >= local.day)) {
    return { year: local.year, month: row.month, day: d };
  }
  return { year: local.year + 1, month: row.month, day: effectiveDay(row.month, row.day, local.year + 1) };
}

/** The age being celebrated, or null when the year is unknown or implies an age below 1. */
export function ageOn(row, celebrationYear) {
  if (!Number.isInteger(row?.year)) return null;
  const age = celebrationYear - row.year;
  return age >= 1 ? age : null;
}

export function isEventEligible(row, local, hour) {
  return local.hour >= clampHour(hour)
    && isBirthdayOn(row, local)
    && (row.lastAnnouncedYear == null || row.lastAnnouncedYear < local.year);
}

/**
 * Splits role markers into the ones whose role is still due today (`keep`) and the ones to remove (`expired`).
 * Pure: no clock inside.
 * @param {Array<{ month: number, day: number, roleGrantedAt: Date }>} markers
 * @param {{ enabled: boolean, roleId: string|null, local: object, tz: string }} opts
 */
export function partitionMarkers(markers, { enabled, roleId, local, tz }) {
  const todayKey = `${String(local.year).padStart(4, '0')}-${pad2(local.month)}-${pad2(local.day)}`;
  const keep = [];
  const expired = [];
  for (const m of markers) {
    const due = Boolean(enabled && roleId) && isBirthdayOn(m, local) && localDateKey(m.roleGrantedAt, tz) === todayKey;
    (due ? keep : expired).push(m);
  }
  return { keep, expired };
}

/** The template for a celebration: the age message when the age is known, otherwise the plain one. */
export function pickTemplate(settings, age) {
  return age == null
    ? (settings?.birthdayMessage || DEFAULT_BIRTHDAY_MESSAGE)
    : (settings?.birthdayMessageAge || DEFAULT_BIRTHDAY_MESSAGE_AGE);
}

/** The first run strictly after `now` that sits just after a local hour boundary of `tz` (works in half- and quarter-hour zones). */
export function nextTickAt(now, tz) {
  const at = toDate(now);
  const p = zonedParts(at, tz);
  const start = zonedToUtc({ year: p.year, month: p.month, day: p.day, hour: p.hour, minute: 0 }, tz);
  let t = start.getTime() + HOUR_MS + BOUNDARY_SLACK_MS;
  // A DST overlap makes zonedToUtc return the earlier instant: the loop catches up.
  while (t <= at.getTime()) t += HOUR_MS;
  return new Date(t);
}

/** Seconds until Jan 1 of the next local year, plus one day. Never below 60. */
export function tombstoneTtlSeconds(now, tz) {
  const at = toDate(now);
  const p = zonedParts(at, tz);
  const end = zonedToUtc({ year: p.year + 1, month: 1, day: 1 }, tz);
  return Math.max(60, Math.ceil((end.getTime() - at.getTime()) / 1000) + 86_400);
}

/**
 * Packs announcement lines into messages, greedily and in order. Lines are never split.
 * @param {Array<{ id: string, userId: string, text: string }>} entries `text` is already clamped to `maxChars`
 * @returns {Array<{ content: string, userIds: string[], ids: string[] }>}
 */
export function chunkAnnouncements(entries, { maxChars = MAX_MESSAGE_CHARS, maxUsers = MAX_USERS_PER_MESSAGE } = {}) {
  const chunks = [];
  let cur = null;
  for (const e of entries) {
    const text = clampText(String(e.text ?? ''), maxChars);
    if (cur && cur.ids.length < maxUsers && cur.content.length + 1 + text.length <= maxChars) {
      cur.content += `\n${text}`;
    } else {
      cur = { content: text, userIds: [], ids: [] };
      chunks.push(cur);
    }
    cur.ids.push(e.id);
    if (!cur.userIds.includes(e.userId)) cur.userIds.push(e.userId);
  }
  return chunks;
}

/** 'March 14', 'Mar 14' (short), 'March 14, 1998' (withYear and a year). */
export function formatBirthday({ month, day, year }, { short = false, withYear = false } = {}) {
  const name = MONTH_NAMES[month - 1] ?? '?';
  let out = `${short ? name.slice(0, 3) : name} ${day}`;
  if (withYear && year) out += `, ${year}`;
  return out;
}

/** A fenced code block that user text cannot break out of: a zero-width space follows every backtick, so no run of three exists. */
export function codeBlock(text, max = 1000) {
  return `\`\`\`\n${clampText(String(text).replace(/`/g, '`​'), max)}\n\`\`\``;
}

// ── Date parsing (spec §6.2) ──────────────────────────────────────────────────

const FORMAT_MESSAGE = 'Use **MM-DD** (like `03-14`) or **YYYY-MM-DD** (like `1998-03-14`).';
const FAIL = (code, message) => ({ ok: false, code, message });

/**
 * Parses and validates a birthday typed by a member or an admin. Layered on foundation's parseBirthday (calendar validity); the year
 * range, Feb 29 for a given year, "required" and "future" rules are added here because foundation's "current year" is not timezone
 * aware and its result does not say why parsing failed.
 * @returns {{ ok: true, month: number, day: number, year: number|null } | { ok: false, code: string, message: string }}
 */
export function parseBirthdayInput(raw, { requireYear = false, now = new Date(), tz = 'UTC' } = {}) {
  const s = String(raw ?? '').trim();
  const m = /^(?:(\d{4})-)?(\d{1,2})-(\d{1,2})$/.exec(s);
  if (!m) return FAIL('FORMAT', FORMAT_MESSAGE);
  const year = m[1] ? Number(m[1]) : null;
  const month = Number(m[2]);
  const day = Number(m[3]);
  const local = zonedParts(now, tz);
  if (year !== null && (year < 1900 || year > local.year)) {
    return FAIL('YEAR_RANGE', `The year must be between 1900 and ${local.year}.`);
  }
  if (parseBirthday(`${pad2(month)}-${pad2(day)}`, { now }) === null) {
    return FAIL('INVALID_DATE', `**${s}** is not a real date.`);
  }
  if (year !== null && day > daysInMonth(year, month)) {
    return FAIL('INVALID_DATE', `**${s}** is not a real date: ${year} is not a leap year.`);
  }
  if (requireYear && year === null) {
    return FAIL('YEAR_REQUIRED', 'This server asks for your birth year too: use **YYYY-MM-DD** (like `1998-03-14`).');
  }
  if (year === local.year && (month > local.month || (month === local.month && day > local.day))) {
    return FAIL('FUTURE', 'That date is in the future.');
  }
  return { ok: true, month, day, year };
}

// ── Message rendering ─────────────────────────────────────────────────────────

/**
 * One announcement line. The `age` variable is empty when unknown. An empty result falls back to the default template.
 */
export function renderBirthdayLine({ template, member, guild, channel, age }) {
  const ctx = { member, user: member?.user, guild, channel, vars: { age: age == null ? '' : String(age) } };
  const out = renderPlaceholders(template, ctx, { maxLength: MAX_MESSAGE_CHARS });
  if (out.trim() !== '') return out;
  return renderPlaceholders(pickTemplate(null, age), ctx, { maxLength: MAX_MESSAGE_CHARS });
}

// ── Database access ───────────────────────────────────────────────────────────

/** @returns {Promise<object|null>} the Birthday row of a member (throws on a DB failure) */
export function getBirthday(guildId, userId) {
  return prisma.birthday.findUnique({ where: { guildId_userId: { guildId, userId } } });
}

/** @returns {Promise<{ total: number, withoutYear: number }>} */
export async function countBirthdays(guildId) {
  const [total, withoutYear] = await Promise.all([
    prisma.birthday.count({ where: { guildId } }),
    prisma.birthday.count({ where: { guildId, year: null } }),
  ]);
  return { total, withoutYear };
}

/** @returns {Promise<boolean>} true when any member of the guild still carries a role marker */
export async function hasMarkers(guildId) {
  const row = await prisma.birthday.findFirst({ where: { guildId, roleGrantedAt: { not: null } }, select: { id: true } });
  return row !== null;
}

/**
 * Creates the Birthday row of a member, or changes its date. Throws on a DB failure (the command answers G2).
 *
 * One celebration per member per guild per local year (spec §6.6): a changed date carries `lastAnnouncedYear` over; forget-then-set is
 * closed by the Redis tombstone (fail-open); a date registered after the announcement hour on the day itself counts from next year.
 * @returns {Promise<{ created: boolean, dateChanged: boolean, late: boolean, alreadyCelebrated: boolean, row: object }>}
 */
export async function saveBirthday({ guild, userId, date: { month, day, year }, settings, now = new Date() }) {
  await ensureGuildSettings(guild.id);
  const tz = resolveTimeZone(settings);
  const local = zonedParts(now, tz);
  const hour = clampHour(settings?.birthdayHour);

  const existing = await getBirthday(guild.id, userId);
  const dateChanged = !existing || existing.month !== month || existing.day !== day;
  let last = existing?.lastAnnouncedYear ?? null;

  if (!existing) {
    const t = await cache.get(tombstoneKey(guild.id, userId));
    if (Number(t) === local.year) last = local.year;
  }

  const late = dateChanged
    && isBirthdayOn({ month, day }, local)
    && local.hour >= hour
    && (last == null || last < local.year);
  if (late) last = local.year;
  const alreadyCelebrated = !late && dateChanged && last === local.year;

  const where = { guildId_userId: { guildId: guild.id, userId } };
  const create = { guildId: guild.id, userId, month, day, year, lastAnnouncedYear: last };
  const update = { month, day, year, lastAnnouncedYear: last };
  let row;
  try {
    row = await prisma.birthday.upsert({ where, create, update });
  } catch (err) {
    if (err?.code !== 'P2002') throw err;
    row = await prisma.birthday.upsert({ where, create, update }); // a concurrent create won: the retry updates
  }
  return { created: !existing, dateChanged, late, alreadyCelebrated, row };
}

/**
 * Deletes a member's birthday. The birthday role is taken off first, because afterwards no marker would remain for the tick to find.
 * The tombstone stays (forget-then-set must not celebrate twice). Throws on a DB failure.
 * @returns {Promise<{ removed: boolean }>}
 */
export async function removeBirthday({ guild, userId, settings }) {
  const row = await getBirthday(guild.id, userId);
  if (!row) return { removed: false };
  const roleId = settings?.birthdayRoleId;
  if (row.roleGrantedAt && roleId) {
    const member = guild.members.cache.get(userId) ?? await guild.members.fetch(userId).catch(() => null);
    if (member?.roles.cache.has(roleId)) {
      await member.roles.remove(roleId, 'Birthday removed').catch((err) => {
        logger.warn(`birthdays: could not remove the birthday role from ${userId} in guild ${guild.id}: ${err?.message ?? err}`);
      });
    }
  }
  await prisma.birthday.deleteMany({ where: { guildId: guild.id, userId } });
  return { removed: true };
}

/**
 * Members whose birthday comes next, soonest first (today included).
 * @returns {Promise<Array<{ userId: string, month: number, day: number, occ: object, at: Date, isToday: boolean }>>}
 */
export async function getUpcomingBirthdays(guild, settings, { limit = NEXT_LIMIT, now = new Date() } = {}) {
  const tz = resolveTimeZone(settings);
  const local = zonedParts(now, tz);
  const select = { userId: true, month: true, day: true };
  const orderBy = [{ month: 'asc' }, { day: 'asc' }, { userId: 'asc' }];
  const m = local.month;
  const d = local.day;

  let rows = await prisma.birthday.findMany({
    where: { guildId: guild.id, OR: [{ month: { gt: m } }, { month: m, day: { gte: d } }] },
    orderBy, take: NEXT_CANDIDATES, select,
  });
  if (rows.length < NEXT_CANDIDATES) {
    const wrapped = await prisma.birthday.findMany({
      where: { guildId: guild.id, OR: [{ month: { lt: m } }, { month: m, day: { lt: d } }] },
      orderBy, take: NEXT_CANDIDATES - rows.length, select,
    });
    rows = rows.concat(wrapped);
  }

  let entries = rows.map((r) => {
    const occ = nextOccurrence(r, local);
    return {
      userId: r.userId,
      month: r.month,
      day: r.day,
      occ,
      at: zonedToUtc({ year: occ.year, month: occ.month, day: occ.day }, tz),
      isToday: occ.year === local.year && occ.month === local.month && occ.day === local.day,
    };
  });
  entries.sort((a, b) => (a.occ.year - b.occ.year) || (a.occ.month - b.occ.month) || (a.occ.day - b.occ.day)
    || (a.userId < b.userId ? -1 : a.userId > b.userId ? 1 : 0));

  // Departed members and bots are left out. A failed member fetch keeps every row (the list is informational).
  const members = await fetchMembers(guild, entries.map((e) => e.userId)).catch(() => null);
  if (members) entries = entries.filter((e) => members.has(e.userId) && !members.get(e.userId).user?.bot);
  return entries.slice(0, limit);
}

// ── Raw SQL (00-overview rule 22) ─────────────────────────────────────────────

/**
 * The tick's at-most-once guard: marks the rows as celebrated this local year and, when a role is configured, sets the role marker.
 * Only rows that are not yet celebrated this year are returned, so a concurrent run can never claim the same row twice.
 */
const claimSql = ({ guildId, ids, year, setMarker }) => Prisma.sql`
  UPDATE "Birthday"
  SET "lastAnnouncedYear" = ${year}, "updatedAt" = ${SQL_NOW}
      ${setMarker ? Prisma.sql`, "roleGrantedAt" = ${SQL_NOW}` : Prisma.empty}
  WHERE "guildId" = ${guildId}
    AND id = ANY(${ids}::text[])
    AND ("lastAnnouncedYear" IS NULL OR "lastAnnouncedYear" < ${year})
  RETURNING id, "userId", "year"`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'birthdays.claim', build: () => claimSql({ guildId: '0', ids: ['sample'], year: 2026, setMarker: false }) },
  { name: 'birthdays.claimWithMarker', build: () => claimSql({ guildId: '0', ids: ['sample'], year: 2026, setMarker: true }) },
]);

/** Claims the rows (spec §6.4 step 6). @returns {Promise<Array<{ id: string, userId: string, year: number|null }>>} */
export function claimBirthdays({ guildId, ids, year, setMarker }) {
  return prisma.$queryRaw(claimSql({ guildId, ids, year, setMarker }));
}

/** Gives claims back after a failed send: the rows are claimed again by the next run. Never throws. */
export async function revertClaims(guildId, ids, year) {
  if (!ids.length) return;
  try {
    await prisma.birthday.updateMany({
      where: { guildId, id: { in: ids }, lastAnnouncedYear: year },
      data: { lastAnnouncedYear: year - 1 },
    });
  } catch (err) {
    logger.error(`birthdays: could not revert ${ids.length} claim(s) in guild ${guildId}`, err);
  }
}

/** Clears one role marker, but only while it still has the value that was read (a marker re-set by a concurrent claim stays). */
export function clearMarker(m) {
  return prisma.birthday.updateMany({ where: { id: m.id, roleGrantedAt: m.roleGrantedAt }, data: { roleGrantedAt: null } });
}

// ── Members and roles ─────────────────────────────────────────────────────────

/**
 * The members for `ids`: the cache first, then the gateway for the misses (chunks of 100). A throw propagates. An id missing from the
 * result has left the guild.
 * @returns {Promise<Map<string, import('discord.js').GuildMember>>}
 */
export async function fetchMembers(guild, ids) {
  const out = new Map();
  const missing = [];
  for (const id of new Set(ids)) {
    const cached = guild.members.cache.get(id);
    if (cached) out.set(id, cached);
    else missing.push(id);
  }
  for (let i = 0; i < missing.length; i += MEMBER_FETCH_CHUNK) {
    const found = await guild.members.fetch({ user: missing.slice(i, i + MEMBER_FETCH_CHUNK), time: MEMBER_FETCH_TIMEOUT_MS });
    for (const [id, member] of found) out.set(id, member);
  }
  return out;
}

/** Adds the role to a member, one role per call. @returns {Promise<boolean>} true when the call succeeded */
async function grantRole(guildId, member, roleId) {
  try {
    await member.roles.add(roleId, 'Birthday');
    return true;
  } catch (err) {
    const code = err?.code;
    if (code === 10007) return false; // the member has left
    if (code === 50013 || code === 50001) warnOnce(guildId, 'role-perms', `birthdays: missing access to grant the birthday role in guild ${guildId}`);
    else logger.warn(`birthdays: could not grant the birthday role in guild ${guildId}: ${err?.message ?? err}`);
    return false;
  }
}

/** @returns {Promise<'ok'|'gone'|'keep'>} 'gone' = nothing left to remove, 'keep' = retry on a later tick */
async function takeRole(guildId, member, roleId, reason) {
  try {
    await member.roles.remove(roleId, reason);
    return 'ok';
  } catch (err) {
    const code = err?.code;
    if (code === 10007 || code === 10011) return 'gone';
    if (code === 50013 || code === 50001) warnOnce(guildId, 'role-perms', `birthdays: missing access to remove the birthday role in guild ${guildId}`);
    else logger.warn(`birthdays: could not remove the birthday role in guild ${guildId}: ${err?.message ?? err}`);
    return 'keep';
  }
}

/**
 * Moves today's celebrants off the previous birthday role (spec §6.8), inside the guild mutex so it never interleaves with a tick.
 * With no new role every marker is cleared. Never throws.
 * @returns {Promise<{ removed: number, failed: number }>}
 */
export function transferBirthdayRole(guild, oldRoleId, newRoleId) {
  return guildMutex.run(guild.id, async () => {
    let removed = 0;
    let failed = 0;
    try {
      const markers = await prisma.birthday.findMany({
        where: { guildId: guild.id, roleGrantedAt: { not: null } },
        select: { id: true, userId: true },
        take: MAX_MARKERS_PER_TICK,
      });
      const old = guild.roles.cache.get(oldRoleId);
      if (old && markers.length && checkAssignableRole(guild, old, { allowDangerous: true }).ok) {
        const members = await fetchMembers(guild, markers.map((m) => m.userId)).catch(() => new Map());
        for (const member of members.values()) {
          if (!member.roles.cache.has(oldRoleId)) continue;
          const res = await takeRole(guild.id, member, oldRoleId, 'Birthday role changed');
          if (res === 'ok') removed += 1;
          else if (res === 'keep') failed += 1;
        }
      } else if (old && markers.length) {
        failed += markers.length; // the old role exists but the bot can no longer manage it
      }
      if (!newRoleId) await prisma.birthday.updateMany({ where: { guildId: guild.id, roleGrantedAt: { not: null } }, data: { roleGrantedAt: null } });
    } catch (err) {
      logger.error(`birthdays: role transfer failed in guild ${guild.id}`, err);
    }
    return { removed, failed };
  });
}

// ── Scheduling (spec §6.3) ────────────────────────────────────────────────────

/**
 * Makes sure the guild has its recurring birthday.tick job when anything is configured, and drops it when nothing is (and no role
 * marker is left to clear). `soon` replaces the job with one that runs in a few seconds (config changes). Never throws.
 * @param {string} guildId
 * @param {object|null} settings
 * @param {{ soon?: boolean }} [opts]
 */
export async function ensureTickJob(guildId, settings, { soon = false } = {}) {
  try {
    if (!settings) return; // without settings we cannot decide: an existing job is kept
    const configured = Boolean(settings.birthdayChannelId || settings.birthdayRoleId);
    if (!configured && !(await hasMarkers(guildId))) {
      await cancelJobs({ dedupeKey: tickKey(guildId) });
      return;
    }
    // Configured, or a role was just cleared and markers remain: let the tick clear them.
    const base = { guildId, type: TICK_JOB_TYPE, payload: {}, dedupeKey: tickKey(guildId), onConflict: 'replace' };
    if (soon) {
      // 'replace' also supersedes a RUNNING row: the version bump makes that run's reschedule a no-op, and the `running` guard
      // defers the new run when it lands while the old one is still executing.
      await scheduleJob({ ...base, runAt: new Date(Date.now() + SOON_MS) });
      return;
    }
    const job = await getJob(tickKey(guildId));
    if (job && (job.status === 'PENDING' || job.status === 'RUNNING')) return;
    await scheduleJob({ ...base, runAt: nextTickAt(new Date(), resolveTimeZone(settings)) }); // revives a missing, FAILED or DONE job
  } catch (err) {
    logger.error('birthdays: could not schedule birthday.tick', err);
  }
}

/** guildReady hook: unconfigured guilds cost nothing at startup; configured ones get a missing or FAILED job back. */
export async function onGuildReady(ctx) {
  const settings = await ctx.getSettings();
  if (!settings) return;
  if (!(settings.birthdayChannelId || settings.birthdayRoleId)) return;
  await ensureTickJob(ctx.guild.id, settings);
}

// ── The tick (spec §6.4) ──────────────────────────────────────────────────────

/**
 * The `birthday.tick` job handler. Never throws: an error becomes a retry in five minutes, so the recurring job is not marked FAILED.
 * @returns {Promise<{ nextRunAt: Date }|undefined>}
 */
export async function runBirthdayTick(job, ctx) {
  const guildId = job.guildId;
  if (running.has(guildId)) return { nextRunAt: new Date(Date.now() + CONTINUE_MS) };
  if (!ctx.guild?.available) return { nextRunAt: new Date(Date.now() + ERROR_RETRY_MS) };
  running.add(guildId);
  try {
    return await guildMutex.run(guildId, () => tickGuild(ctx.client, ctx.guild));
  } catch (err) {
    logger.error(`birthday.tick failed for guild ${guildId}`, err);
    return { nextRunAt: new Date(Date.now() + ERROR_RETRY_MS) };
  } finally {
    running.delete(guildId);
  }
}

/**
 * One tick for one guild: remove expired roles, claim and announce today's birthdays, grant roles, keep roles of today's members.
 * Order: removal, claim, announce, tombstone, grant, maintenance. Every step is idempotent.
 * @returns {Promise<{ nextRunAt: Date }|undefined>} undefined deletes the job (nothing is configured and no marker is left)
 */
export async function tickGuild(client, guild, { now = new Date() } = {}) {
  // 1. Setup.
  const settings = await getSettings(guild.id);
  const tz = resolveTimeZone(settings);
  const local = zonedParts(now, tz);
  const hour = clampHour(settings.birthdayHour);
  const enabled = isModuleEnabled(settings, 'birthdays');
  const deadline = Date.now() + TICK_DEADLINE_MS;
  const outOfTime = () => Date.now() >= deadline;
  let more = false;

  // 2. Resolve the configured channel and role. With the Guilds intent every channel is cached, so a miss means it was deleted.
  let channel = null;
  if (settings.birthdayChannelId) {
    channel = guild.channels.cache.get(settings.birthdayChannelId) ?? null;
    if (!channel) warnOnce(guild.id, 'channel-deleted', `birthdays: the birthday channel of guild ${guild.id} was deleted`);
  }
  let channelOk = false;
  if (channel) {
    channelOk = checkChannelUsable(channel, { needs: [...ANNOUNCE_NEEDS] }).ok;
    if (!channelOk) warnOnce(guild.id, 'channel-perms', `birthdays: cannot post in the birthday channel of guild ${guild.id}`);
  }
  const roleId = settings.birthdayRoleId ?? null;
  let role = null;
  if (roleId) {
    role = guild.roles.cache.get(roleId) ?? null;
    if (!role) warnOnce(guild.id, 'role-deleted', `birthdays: the birthday role of guild ${guild.id} was deleted`);
  }
  // Re-checked every tick: a role that later gained dangerous permissions is never granted again.
  const canGrant = role ? checkAssignableRole(guild, role, { allowDangerous: false }).ok : false;
  const canRemove = role ? checkAssignableRole(guild, role, { allowDangerous: true }).ok : false;
  if (role && !canGrant) warnOnce(guild.id, 'role-perms', `birthdays: cannot grant the birthday role of guild ${guild.id}`);

  // 3. Read.
  const markers = await prisma.birthday.findMany({
    where: { guildId: guild.id, roleGrantedAt: { not: null } },
    orderBy: { roleGrantedAt: 'asc' },
    take: MAX_MARKERS_PER_TICK,
    select: { id: true, userId: true, month: true, day: true, roleGrantedAt: true },
  });
  const { keep, expired } = partitionMarkers(markers, { enabled, roleId: role ? roleId : null, local, tz });

  // 4. Members. A throw propagates before anything was changed. An id missing from the map has left the guild.
  const members = new Map();
  const asked = new Set();
  const loadMembers = async (userIds) => {
    const wanted = [...new Set(userIds)].filter((id) => !asked.has(id));
    if (!wanted.length) return;
    for (const id of wanted) asked.add(id);
    for (const [id, member] of await fetchMembers(guild, wanted)) members.set(id, member);
  };
  const markerIds = [...keep, ...expired].map((r) => r.userId);

  // Today's candidates, in pages by userId. Members who left (or bots) are never claimed, so a later tick re-checks them, but they
  // are paged past here: 50+ of them sharing today's date must not fill the batch and starve the members behind them.
  const candidates = [];
  if (enabled && (channel || role) && local.hour >= hour) {
    const where = {
      guildId: guild.id,
      AND: [
        { OR: todayMatchers(local) },
        { OR: [{ lastAnnouncedYear: null }, { lastAnnouncedYear: { lt: local.year } }] },
      ],
    };
    let cursor = null;
    let first = true;
    scan: for (;;) {
      const page = await prisma.birthday.findMany({
        where: cursor === null ? where : { ...where, userId: { gt: cursor } },
        orderBy: { userId: 'asc' },
        take: MAX_EVENTS_PER_TICK + 1,
        select: { id: true, userId: true, month: true, day: true, year: true },
      });
      if (!page.length) break;
      cursor = page[page.length - 1].userId;
      await loadMembers(first ? [...page.map((r) => r.userId), ...markerIds] : page.map((r) => r.userId)); // one gateway request for the first page
      first = false;
      for (const row of page) {
        const member = members.get(row.userId);
        if (!member || member.user?.bot) continue;
        if (candidates.length >= MAX_EVENTS_PER_TICK) { more = true; break scan; }
        candidates.push(row);
      }
      if (page.length <= MAX_EVENTS_PER_TICK) break; // the last page
      if (outOfTime()) { more = true; break; }
    }
  }
  await loadMembers(markerIds);

  // 5. Expired markers: remove the role (also while the module is off).
  for (const m of expired) {
    if (outOfTime()) { more = true; break; }
    const member = members.get(m.userId);
    if (!member || !role) { await clearMarker(m); continue; }
    if (!canRemove) {
      if (now.getTime() - toDate(m.roleGrantedAt).getTime() > MARKER_GIVE_UP_MS) {
        await clearMarker(m);
        logger.warn(`birthdays: gave up removing the birthday role from ${m.userId} in guild ${guild.id}`);
      }
      continue;
    }
    if (!member.roles.cache.has(roleId)) { await clearMarker(m); continue; }
    const res = await takeRole(guild.id, member, roleId, 'Birthday is over');
    if (res !== 'keep') await clearMarker(m);
  }

  // 6. Claim today's celebrations (`candidates` holds only present, non-bot members; the others stay unclaimed, so later ticks re-check them).
  let claimed = [];
  if (candidates.length) {
    claimed = await claimBirthdays({ guildId: guild.id, ids: candidates.map((r) => r.id), year: local.year, setMarker: Boolean(role) });
  }

  // 7. Announce. Without a usable channel the claims are completed silently (the role is still granted; /birthday-config show reports it).
  let completed = claimed;
  if (claimed.length && channel && channelOk) {
    completed = [];
    const entries = claimed.map((r) => {
      const age = ageOn(r, local.year);
      return {
        id: r.id,
        userId: r.userId,
        text: renderBirthdayLine({ template: pickTemplate(settings, age), member: members.get(r.userId), guild, channel, age }),
      };
    });
    const chunks = chunkAnnouncements(entries);
    for (let i = 0; i < chunks.length; i += 1) {
      let failure = null;
      if (outOfTime()) {
        more = true;
        failure = 'deadline';
      } else {
        try {
          await channel.send({ content: chunks[i].content, allowedMentions: { parse: [], users: chunks[i].userIds } });
        } catch (err) {
          failure = err;
        }
      }
      if (failure === null) {
        completed.push(...claimed.filter((r) => chunks[i].ids.includes(r.id)));
        continue;
      }
      if (failure !== 'deadline') {
        const code = failure?.code;
        if (code === 50013 || code === 50001 || code === 10003) {
          warnOnce(guild.id, 'channel-perms', `birthdays: cannot post in the birthday channel of guild ${guild.id} (${code})`);
        } else {
          logger.error(`birthdays: announcing in guild ${guild.id} failed`, failure);
        }
      }
      // Revert this chunk and every unsent one; the next run claims them again.
      await revertClaims(guild.id, chunks.slice(i).flatMap((c) => c.ids), local.year);
      break;
    }
  }

  // 8. Tombstones for completed rows (fail-open).
  const ttl = tombstoneTtlSeconds(now, tz);
  for (const r of completed) await cache.set(tombstoneKey(guild.id, r.userId), local.year, ttl);

  // 9 + 10. Grant the role to claimed rows (completed or reverted), then keep it on today's other celebrants.
  const granted = new Set();
  const ensureRole = async (userId) => {
    const member = members.get(userId);
    if (!member || granted.has(userId)) return;
    granted.add(userId);
    if (member.roles.cache.has(roleId) || member.pending) return;
    await grantRole(guild.id, member, roleId);
  };
  if (canGrant) {
    for (const r of claimed) {
      if (outOfTime()) { more = true; break; }
      await ensureRole(r.userId);
    }
    for (const m of keep) {
      if (outOfTime()) { more = true; break; }
      if (!members.has(m.userId)) { await clearMarker(m); continue; }
      await ensureRole(m.userId);
    }
  }

  // 11. Next run.
  if (more) return { nextRunAt: new Date(Date.now() + CONTINUE_MS) };
  if (!(settings.birthdayChannelId || settings.birthdayRoleId) && !(await hasMarkers(guild.id))) return undefined;
  return { nextRunAt: nextTickAt(new Date(), tz) };
}
