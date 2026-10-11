// Descriptor of the `levels` collection (Levels module): the XP leaderboard of the server (UserLevel rows), with what
// /xp give | remove | set | reset does on each member. The XP changes run in the bot (task `xp.adjust`,
// src/services/dashboardTasks/members.js) because reward roles and the `levelUp` hook need the gateway.
// Also home of the pure helpers the three member lists share (levels, balances, cases): member names and avatars from
// ctx.discord, the member search and the page cursors. Pure: nothing is read or connected at import time.
import { XP_MAX, xpForLevel } from '../../config/constants.js';
import { RecordError } from './errors.js';
import { userAvatarUrl } from './embeds.js';

// ── Shared by the member lists ───────────────────────────────────────────────

/** Rows per page of a member list (the API answers `nextCursor` while there are more). */
export const PAGE_SIZE = 25;

const SNOWFLAKE_RE = /^\d{17,20}$/;
const RANK_CURSOR_RE = /^(\d{1,10})_(\d{17,20})$/;

export const isUserId = (v) => typeof v === 'string' && SNOWFLAKE_RE.test(v);

/** `1,234` */
export const fmt = (n) => Number(n ?? 0).toLocaleString('en-US');

/** The cursor of a list ordered by a number (descending) then the user id (ascending): `<value>_<userId>`. */
export const rankCursor = (value, userId) => `${value}_${userId}`;

/**
 * Reads a rankCursor; anything else is null (the first page).
 * @returns {{ value: number, userId: string }|null}
 */
export function readRankCursor(cursor) {
  const m = RANK_CURSOR_RE.exec(String(cursor ?? ''));
  if (!m) return null;
  const value = Number(m[1]);
  return Number.isSafeInteger(value) ? { value, userId: m[2] } : null;
}

/**
 * The Prisma `where` for the rows after a rankCursor in "value desc, userId asc" order.
 * @param {string} column 'xp' | 'balance'
 */
export function afterRankCursor(column, cursor) {
  const c = readRankCursor(cursor);
  if (!c) return {};
  return { OR: [{ [column]: { lt: c.value } }, { [column]: c.value, userId: { gt: c.userId } }] };
}

/**
 * Names and avatars of users (ctx.discord.getUsers, cached by the dashboard for an hour). Never throws: without Discord
 * the lists show ids and default avatars.
 * @param {object|null|undefined} discord ctx.discord
 * @param {string[]} ids
 * @param {object} [logger]
 * @returns {Promise<Map<string, { id: string, username: string, globalName: string|null, avatarUrl: string, bot: boolean }|null>>}
 */
export async function loadProfiles(discord, ids, logger = null) {
  const wanted = [...new Set((ids ?? []).filter(isUserId))].slice(0, 100);
  if (!wanted.length || typeof discord?.getUsers !== 'function') return new Map();
  try {
    const map = await discord.getUsers(wanted);
    return map instanceof Map ? map : new Map();
  } catch (err) {
    logger?.warn?.(`Dashboard: member names could not be loaded: ${err?.message ?? err}`);
    return new Map();
  }
}

/**
 * The user ids a list search means: the id itself when `query` is one, else the members whose name starts with it
 * (Discord's member search: members only). Without a Discord client only ids can be searched.
 * @returns {Promise<string[]>}
 * @throws {RecordError} discord when Discord cannot answer
 */
export async function searchUserIds(discord, guildId, query) {
  const q = String(query ?? '').replace(/^<@!?(\d{17,20})>$/, '$1').trim();
  if (!q) return [];
  if (isUserId(q)) return [q];
  if (typeof discord?.searchMembers !== 'function') return [];
  try {
    const found = await discord.searchMembers(guildId, q);
    return [...new Set((Array.isArray(found) ? found : []).map((m) => m?.id).filter(isUserId))];
  } catch {
    throw new RecordError('discord', 'Discord did not answer the member search. Try again in a moment.');
  }
}

/** "Alice" (global name, else username); `fallback` (the id) for an unknown user. */
export function profileName(profile, fallback) {
  if (!profile) return String(fallback ?? 'Unknown user');
  return profile.globalName || profile.username || String(fallback ?? 'Unknown user');
}

/** "Alice (@alice)" / "@alice", or "Unknown user (id)". */
export function profileLabel(profile, id) {
  if (!profile) return `Unknown user (${id})`;
  const at = profile.username ? `@${profile.username}` : String(id);
  return profile.globalName && profile.globalName !== profile.username ? `${profile.globalName} (${at})` : at;
}

/** The avatar to show for a user: theirs, else Discord's default for the id. */
export const avatarFor = (profile, id) => (typeof profile?.avatarUrl === 'string' && profile.avatarUrl ? profile.avatarUrl : userAvatarUrl({ id }));

/** A Date as an ISO string, else null. */
export const toIso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

/** Shown when the bot was still working on a create when the dashboard stopped waiting (there is no item to show yet). */
export const PENDING_CREATE = 'The bot is still working on it. Refresh the list in a moment to see the result.';

// ── Level maths (the same sums as xpService: levelFromXp, xpIntoLevel) ───────

/**
 * Level, XP into the level and XP the level needs, for a total.
 * @param {number} total
 * @returns {{ level: number, into: number, need: number }}
 */
export function levelProgress(total) {
  let remaining = Number.isFinite(Number(total)) && Number(total) > 0 ? Math.trunc(Number(total)) : 0;
  let level = 0;
  for (; level < 2000 && remaining >= xpForLevel(level); level++) remaining -= xpForLevel(level);
  return { level, into: remaining, need: xpForLevel(level) };
}

// ── The collection ───────────────────────────────────────────────────────────

/** /xp give and /xp remove take 1 to 1,000,000 XP at a time; /xp set takes 0 to XP_MAX. */
export const XP_STEP_MAX = 1_000_000;

const amountField = (label, description) => Object.freeze({
  field: 'amount', type: 'int', label, description, min: 1, max: XP_STEP_MAX, unit: 'XP', default: 100, required: true,
});

const fields = Object.freeze([
  Object.freeze({
    field: 'userId',
    type: 'user',
    label: 'Member',
    description: 'The member who gets the XP. Bots never earn XP.',
    default: null,
    required: true,
  }),
  amountField('XP to give', 'Added to their total (at most 1,000,000 at a time). Reward roles follow the new level; no level-up message is posted.'),
]);

const readOnly = Object.freeze([
  Object.freeze({ field: 'avatarUrl', type: 'url', label: 'Avatar', nullable: true, default: null }),
  Object.freeze({ field: 'name', type: 'text', label: 'Member', maxLength: 200, default: '' }),
  Object.freeze({ field: 'summary', type: 'text', label: 'Standing', maxLength: 200, default: '' }),
  Object.freeze({ field: 'rank', type: 'int', label: 'Rank', description: 'Position on the leaderboard.', min: 1, max: 2_147_483_647, nullable: true, default: null }),
  Object.freeze({ field: 'level', type: 'int', label: 'Level', min: 0, max: 2000, default: 0 }),
  Object.freeze({ field: 'xp', type: 'int', label: 'Total XP', min: 0, max: XP_MAX, unit: 'XP', default: 0 }),
  Object.freeze({ field: 'progress', type: 'text', label: 'Progress to the next level', maxLength: 100, default: '' }),
  Object.freeze({ field: 'messages', type: 'int', label: 'Messages counted', min: 0, max: 2_147_483_647, default: 0 }),
  Object.freeze({ field: 'voiceMinutes', type: 'int', label: 'Voice time', min: 0, max: 2_147_483_647, unit: 'minutes', default: 0 }),
  Object.freeze({ field: 'lastXpAt', type: 'datetime', label: 'Last XP earned', nullable: true, default: null }),
]);

export const collection = Object.freeze({
  key: 'levels',
  module: 'leveling',
  label: 'Member levels',
  singular: 'member',
  description: 'The XP leaderboard of this server. Give, remove or set XP and reset members like /xp does: reward roles follow '
    + 'the new level and no level-up message is posted. This works while the Levels module is off. Giving XP to every member '
    + 'of a role is only available with /xp give in Discord.',
  limit: null,
  searchable: true,
  searchPlaceholder: 'Search by member name or user ID',
  titleField: 'name',
  subtitleField: 'summary',
  avatarField: 'avatarUrl',
  canEdit: false,
  canDelete: false,
  fields,
  createOnly: Object.freeze(['userId', 'amount']),
  readOnly,
  actions: Object.freeze([
    Object.freeze({
      id: 'give',
      label: 'Give XP',
      description: 'Adds XP to this member (like /xp give).',
      fields: Object.freeze([amountField('XP to give', 'At most 1,000,000 at a time.')]),
    }),
    Object.freeze({
      id: 'take',
      label: 'Remove XP',
      description: 'Takes XP from this member, never below 0 (like /xp remove).',
      fields: Object.freeze([amountField('XP to remove', 'At most 1,000,000 at a time.')]),
    }),
    Object.freeze({
      id: 'set',
      label: 'Set XP',
      description: "Sets this member's total XP (like /xp set).",
      fields: Object.freeze([Object.freeze({
        field: 'xp', type: 'int', label: 'Total XP', description: `From 0 to ${fmt(XP_MAX)}.`, min: 0, max: XP_MAX, unit: 'XP', default: 0,
        required: true,
      })]),
    }),
    Object.freeze({
      id: 'reset',
      label: 'Reset XP',
      description: "Wipes this member's XP and level (like /xp reset).",
      danger: true,
      confirm: "Reset this member's XP and level to zero? This cannot be undone.",
      fields: Object.freeze([]),
    }),
  ]),
});

// ── Items ────────────────────────────────────────────────────────────────────

/**
 * A UserLevel row as a list item. The level is computed from the XP (the stored level can lag behind during a race,
 * as /xp view and /rank do).
 * @param {{ userId: string, xp: number, messageCount?: number, voiceMinutes?: number, lastXpAt?: Date|null }} row
 * @param {{ rank?: number|null, profile?: object|null }} [extra]
 */
export function levelItem(row, { rank = null, profile = null } = {}) {
  const { level, into, need } = levelProgress(row.xp);
  const name = profileName(profile, row.userId);
  return {
    id: row.userId,
    userId: row.userId,
    amount: null,
    avatarUrl: avatarFor(profile, row.userId),
    name,
    summary: [`Level ${level}`, `${fmt(row.xp)} XP`, rank ? `#${fmt(rank)}` : null].filter(Boolean).join(' · '),
    rank,
    level,
    xp: row.xp,
    progress: `${fmt(into)} / ${fmt(need)} XP`,
    messages: row.messageCount ?? 0,
    voiceMinutes: row.voiceMinutes ?? 0,
    lastXpAt: toIso(row.lastXpAt),
  };
}

/** The item of a member whose XP row is gone (after a reset). */
export const emptyLevelItem = (userId, profile = null) => levelItem({ userId, xp: 0, messageCount: 0, voiceMinutes: 0, lastXpAt: null }, { profile });
