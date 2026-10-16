// Achievements service (pkg-achievements.md): tiered milestones (messages, reactions, level, boost, purchases)
// with role / XP / coin rewards and announcements. All logic lives here; the plugin only wires hooks to these
// exports and the two commands only parse options and render replies.
//
// Design in one paragraph: every counted event is ONE atomic UPDATE per member (recordProgress, Phase 1); messages are
// first summed in memory per member and written as one +n UPDATE every few seconds (the message buffer below). The
// first event of a member per achievement INSERTs the rows (Phase 2, ON CONFLICT DO NOTHING). A tier unlock is
// claimed with a conditional updateMany (compare-and-set on `tier`) BEFORE anything is paid, so each tier pays
// out at most once per member (spec D9). Payout and announcements run fire-and-forget so later hooks are never
// held up. There is deliberately NO keyed mutex on the progress path: an XP reward re-enters this module through
// the levels `levelUp` hook and would deadlock (spec §6.4). `configLocks` guards config writes only.
//
// Nothing here runs at import: no timers, no I/O, no registration. Module-scope Maps and throttles only; the message
// buffer starts a timer per waiting member when a message arrives.
import { randomUUID } from 'node:crypto';
import { escapeMarkdown } from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import logger from '../utils/logger.js';
import { cache } from './redis.js';
import { ensureGuildSettings } from './settingsService.js';
import { grant, hasProvider, isRewardAvailable, providerModule } from './rewards.js';
import { createThrottle, createKeyedMutex } from '../utils/async.js';
import { LIMITS } from '../config/constants.js';
import { MODULES } from '../config/modules.js';
import { renderTemplate } from '../utils/customEmbed.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { checkAssignableRole, checkChannelUsable } from '../utils/interactionHelpers.js';
import { clampText, formatNumber, progressBar, joinWithLimit } from '../utils/text.js';
import { parseDateInput, discordTimestamp } from '../utils/time.js';

// ── Constants (spec §3.6; every default text, goal and emoji below is OUR choice, not MEE6's) ───────────────

export const TRIGGERS = Object.freeze(['MESSAGES', 'REACTIONS', 'LEVEL', 'BOOST', 'PURCHASES']);
export const TRIGGER_LABELS = Object.freeze({ MESSAGES: 'Messages sent', REACTIONS: 'Reactions added', LEVEL: 'Level reached', BOOST: 'Server boosts', PURCHASES: 'Shop items bought' });
export const TRIGGER_EMOJI = Object.freeze({ MESSAGES: '💬', REACTIONS: '😄', LEVEL: '⭐', BOOST: '💎', PURCHASES: '🛒' });
export const TRIGGER_UNITS = Object.freeze({ MESSAGES: ['message', 'messages'], REACTIONS: ['reaction', 'reactions'], LEVEL: ['level', 'levels'], BOOST: ['boost', 'boosts'], PURCHASES: ['item bought', 'items bought'] });
export const TIER_NAMES = Object.freeze(['Bronze', 'Silver', 'Gold', 'Diamond']);
export const TIER_EMOJIS = Object.freeze(['🥉', '🥈', '🥇', '💎']);
export const REWARD_TYPES = Object.freeze(['ROLE_ADD', 'ROLE_REMOVE', 'XP', 'COINS']);
export const GOAL_MAX = Object.freeze({ MESSAGES: 10_000_000, REACTIONS: 10_000_000, LEVEL: 1000, BOOST: 100, PURCHASES: 1_000_000 });
export const GOAL_INPUT_MAX = 10_000_000;          // builder max for integer goal options
export const PROGRESS_MAX = 1_000_000_000;         // int4-safe clamp
export const MAX_REWARD_AMOUNT = 1_000_000;        // per reward, XP or coins
export const NAME_MAX = 50, DESCRIPTION_MAX = 200, MESSAGE_MAX = 1500;
export const DEFAULT_ACHIEVEMENT_MESSAGE = '🏆 {player} unlocked **{achievement}**!';
export const ALMOST_THERE_MESSAGE = "🔥 Almost there! You're at **{progress}/{goal}** for **{achievement}** in **{server}**.";
export const ACH_CACHE_TTL_S = 300, ACH_MEMO_TTL_MS = 30_000, MEMO_MAX = 10_000, SEEDED_MAX = 100_000;
export const REACTION_DEDUPE_TTL_S = 86_400, BOOST_DEDUPE_TTL_S = 86_400, BOOST_FRESH_MS = 60_000;
export const MAX_PURCHASE_DELTA = 1000;
export const DEADLINE_MIN_MS = 60_000, DEADLINE_MAX_MS = 5 * 365 * 86_400_000;
export const MEMBER_PAGE_SIZE = 6, LIST_PAGE_SIZE = 10;
export const MESSAGE_HOOK_TRIGGERS = Object.freeze(['MESSAGES', 'LEVEL', 'BOOST']);
export const achievementsKey = (guildId) => `guild:${guildId}:achievements`;
export const PRESETS = Object.freeze({
  chatterbox: Object.freeze({ name: 'Chatterbox', description: 'Send messages in the server', emoji: '💬', trigger: 'MESSAGES', goals: [100, 500, 2500, 10000] }),
  booster: Object.freeze({ name: 'Booster', description: 'Boost the server', emoji: '💎', trigger: 'BOOST', goals: [1] }),
  level10: Object.freeze({ name: 'Level 10', description: 'Reach level 10', emoji: '⭐', trigger: 'LEVEL', goals: [10] }),
});
export const MODULE_OFF_NOTE = '\n\n⚠️ The Achievements module is off, so no progress is counted. Turn it on with `/module enable achievements`.';

const ALMOST_RATIO = 0.9;
const SNOWFLAKE_RE = /^\d{17,20}$/;

// ── In-process state (per shard; a guild's commands and events always land on its own shard) ────────────────

/** @type {Map<string, { at: number, byTrigger: Record<string, object[]> }>} guildId -> built index */
const indexMemo = new Map();
/** @type {Map<string, Promise<object|null>>} in-flight index loads, so a stale memo causes one query, not one per message */
const indexLoading = new Map();
/** @type {Map<string, true>} `${achievementId}:${userId}` already caught up (insertion order = eviction order) */
const seeded = new Map();
const warnThrottle = createThrottle(10 * 60_000);
const configLocks = createKeyedMutex();

/** Test helper: forgets every in-process cache (waiting message counts are dropped, not written). */
export function __resetAchievementState() {
  indexMemo.clear();
  indexLoading.clear();
  seeded.clear();
  for (const entry of pendingMessages.values()) clearTimeout(entry.timer);
  pendingMessages.clear();
  bufferStopped = false;
}

/** Runs `fn` after every earlier config write of the same guild (config only; never the progress or payout path). */
export function withConfigLock(guildId, fn) {
  return configLocks.run(`cfg:${guildId}`, fn);
}

function warnOnce(guildId, kind, message, err) {
  if (!warnThrottle(`${guildId}:${kind}`)) return;
  if (err) logger.warn(message, err);
  else logger.warn(message);
}

function markSeeded(achievementId, userId) {
  const key = `${achievementId}:${userId}`;
  seeded.delete(key);
  seeded.set(key, true);
  while (seeded.size > SEEDED_MAX) seeded.delete(seeded.keys().next().value);
}

const toInt = (n, min, max) => {
  const v = Math.trunc(Number(n));
  if (!Number.isFinite(v)) return min;
  return Math.min(max, Math.max(min, v));
};

// ── Rewards and tiers (spec §3.2) ────────────────────────────────────────────────────────────────────────

/**
 * Normalises a reward list: keeps valid rewards only, one per type, ROLE_REMOVE dropped when it equals the
 * ROLE_ADD role, ordered ROLE_ADD, ROLE_REMOVE, XP, COINS, with only the relevant keys. Never throws.
 * @param {unknown} raw
 * @returns {Array<{ type: string, roleId?: string, amount?: number }>}
 */
export function normalizeRewards(raw) {
  if (!Array.isArray(raw)) return [];
  const byType = new Map();
  for (const r of raw) {
    if (!r || typeof r !== 'object' || !REWARD_TYPES.includes(r.type) || byType.has(r.type)) continue;
    if (r.type === 'ROLE_ADD' || r.type === 'ROLE_REMOVE') {
      if (typeof r.roleId !== 'string' || !SNOWFLAKE_RE.test(r.roleId)) continue;
      byType.set(r.type, { type: r.type, roleId: r.roleId });
    } else {
      if (!Number.isInteger(r.amount) || r.amount < 1 || r.amount > MAX_REWARD_AMOUNT) continue;
      byType.set(r.type, { type: r.type, amount: r.amount });
    }
  }
  const add = byType.get('ROLE_ADD');
  const remove = byType.get('ROLE_REMOVE');
  if (add && remove && add.roleId === remove.roleId) byType.delete('ROLE_REMOVE');
  return REWARD_TYPES.filter((t) => byType.has(t)).map((t) => byType.get(t));
}

/**
 * Normalises the stored tiers JSON (version 1 = a bare array). Pure, never throws, used on every read.
 * @param {unknown} raw
 * @returns {Array<{ name: string, goal: number, rewards: object[] }>}
 */
export function normalizeTiers(raw) {
  if (!Array.isArray(raw)) return [];
  const valid = raw
    .filter((t) => t && typeof t === 'object' && Number.isInteger(t.goal) && t.goal >= 1 && t.goal <= GOAL_INPUT_MAX)
    .slice(0, LIMITS.ACHIEVEMENT_TIERS);
  const out = [];
  for (const t of valid) {
    if (out.length && t.goal <= out[out.length - 1].goal) break;
    out.push({ name: TIER_NAMES[out.length], goal: t.goal, rewards: normalizeRewards(t.rewards) });
  }
  return out;
}

/**
 * Checks tiers before every write.
 * @param {Array<{ goal: number, rewards?: object[] }>} tiers
 * @param {string} trigger
 * @returns {{ ok: true } | { ok: false, error: string }}
 */
export function validateTiers(tiers, trigger) {
  if (!Array.isArray(tiers) || tiers.length < 1 || tiers.length > LIMITS.ACHIEVEMENT_TIERS) {
    return { ok: false, error: `An achievement needs 1 to ${LIMITS.ACHIEVEMENT_TIERS} tiers.` };
  }
  const max = GOAL_MAX[trigger];
  if (!max) return { ok: false, error: 'Unknown trigger.' };
  let prev = 0;
  for (const t of tiers) {
    if (!Number.isInteger(t?.goal) || t.goal < 1) return { ok: false, error: 'Goals must be whole numbers of at least 1.' };
    if (t.goal > max) return { ok: false, error: `${TRIGGER_LABELS[trigger]} goals can be at most ${formatNumber(max)}.` };
    if (t.goal <= prev) return { ok: false, error: 'Goals must go up from tier to tier.' };
    prev = t.goal;
    const rewards = t.rewards ?? [];
    if (JSON.stringify(normalizeRewards(rewards)) !== JSON.stringify(rewards)) return { ok: false, error: 'Invalid rewards.' };
  }
  return { ok: true };
}

/** Writes always store the normalised form. */
export function serializeTiers(tiers) {
  return normalizeTiers(tiers);
}

/**
 * Replaces the existing reward of every given type with the incoming one; the rest is kept.
 * @param {object[]} existing
 * @param {object[]} incoming
 * @param {Iterable<string>} givenTypes
 */
export function mergeRewards(existing, incoming, givenTypes) {
  const given = new Set(givenTypes ?? []);
  return normalizeRewards([...(existing ?? []).filter((r) => !given.has(r?.type)), ...(incoming ?? [])]);
}

/** `remapTierAfterRemoval` documents the SQL in applyTierChange: members past the removed tier lose one count. */
export function remapTierAfterRemoval(tier, k) {
  return tier > k ? tier - 1 : tier;
}

// ── Input parsing ────────────────────────────────────────────────────────────────────────────────────────

/**
 * Parses `100` or `100,500,1k` (separators: space , ; > | /). Suffix k/m multiply by 1e3/1e6.
 * @param {unknown} input
 * @param {string} trigger
 * @returns {{ ok: true, goals: number[] } | { ok: false, error: string }}
 */
export function parseGoals(input, trigger) {
  const tokens = String(input ?? '').trim().split(/[\s,;>|/]+/).filter(Boolean);
  if (tokens.length < 1 || tokens.length > LIMITS.ACHIEVEMENT_TIERS) {
    return { ok: false, error: 'Give 1 to 4 goals, lowest first, e.g. `100,500,1k`.' };
  }
  const max = GOAL_MAX[trigger] ?? GOAL_INPUT_MAX;
  const goals = [];
  for (const tok of tokens) {
    const m = /^(\d+(?:\.\d+)?)([km])?$/i.exec(tok);
    if (!m) return { ok: false, error: `\`"${tok}"\` is not a whole number.` };
    const mult = m[2] ? (m[2].toLowerCase() === 'k' ? 1e3 : 1e6) : 1;
    const value = Number(m[1]) * mult;
    const rounded = Math.round(value);
    // 1.1k is 1100.0000000000002 in floating point; only a real fraction (1.0005k = 1000.5) is refused.
    if (!Number.isFinite(value) || Math.abs(value - rounded) > 1e-6) return { ok: false, error: `\`"${tok}"\` is not a whole number.` };
    if (rounded < 1) return { ok: false, error: 'Goals must be at least 1.' };
    if (rounded > max) return { ok: false, error: `${TRIGGER_LABELS[trigger] ?? 'Goals'} goals can be at most ${formatNumber(max)}.` };
    goals.push(rounded);
  }
  for (let i = 1; i < goals.length; i++) {
    if (goals[i] <= goals[i - 1]) return { ok: false, error: 'Goals must go up from tier to tier, e.g. `100,500,1k`.' };
  }
  return { ok: true, goals };
}

/** Trim, drop control characters, collapse whitespace; 1 to 50 characters or null. */
export function normalizeAchievementName(s) {
  const out = String(s ?? '').trim().replace(/[\u0000-\u001f\u007f]/g, '').replace(/\s+/g, ' ').trim();
  return out.length >= 1 && out.length <= NAME_MAX ? out : null;
}

// 1 to 32 name characters (the spec's own test uses a one-letter name); the id decides whether the bot can show it.
const CUSTOM_EMOJI_RE = /^<a?:[A-Za-z0-9_]{1,32}:(\d{17,20})>$/;
const UNICODE_EMOJI_RE = /^[\p{Extended_Pictographic}\p{Emoji_Component}‍️\p{Regional_Indicator}]+$/u;

/**
 * Validates a badge emoji: one Unicode emoji or a custom emoji from a server the bot is in.
 * @param {unknown} s
 * @param {{ hasCustomEmoji?: (id: string) => boolean, allowClear?: boolean }} [opts]
 * @returns {{ ok: true, value: string|null } | { ok: false, error: string }}
 */
export function normalizeBadgeEmoji(s, { hasCustomEmoji = () => false, allowClear = false } = {}) {
  const v = String(s ?? '').trim();
  if (allowClear && v.toLowerCase() === 'none') return { ok: true, value: null };
  const custom = CUSTOM_EMOJI_RE.exec(v);
  if (custom) {
    return hasCustomEmoji(custom[1])
      ? { ok: true, value: v }
      : { ok: false, error: "I can only show custom emojis from servers I'm in." };
  }
  if (v && v.length <= 32 && !/\s/.test(v) && UNICODE_EMOJI_RE.test(v)
    && (/\p{Extended_Pictographic}/u.test(v) || (v.match(/\p{Regional_Indicator}/gu) ?? []).length >= 2)) {
    return { ok: true, value: v };
  }
  return { ok: false, error: 'Use one emoji, like 🏆, or a custom emoji from this server.' };
}

/**
 * Parses a deadline: a date in the server time zone (`2026-12-31 23:59`) or a duration (`30d`).
 * @param {unknown} input
 * @param {string} tz
 * @param {{ now?: Date, allowClear?: boolean }} [opts]
 * @returns {{ ok: true, value: Date|null } | { ok: false, error: string }}
 */
export function parseDeadline(input, tz, { now = new Date(), allowClear = false } = {}) {
  const s = String(input ?? '').trim();
  if (allowClear && ['none', 'off', 'clear'].includes(s.toLowerCase())) return { ok: true, value: null };
  const zone = tz || 'UTC';
  const value = parseDateInput(s, zone, { now });
  if (!(value instanceof Date) || Number.isNaN(value.getTime())) {
    return { ok: false, error: `Use a date like 2026-12-31 23:59 (server time zone: ${zone}) or a duration like 30d.` };
  }
  const delta = value.getTime() - now.getTime();
  if (delta < DEADLINE_MIN_MS) return { ok: false, error: 'The deadline must be in the future.' };
  if (delta > DEADLINE_MAX_MS) return { ok: false, error: 'The deadline can be at most 5 years away.' };
  return { ok: true, value };
}

/**
 * Why a reward kind cannot be configured right now, or null when it can.
 * @param {'xp'|'coins'} kind
 * @param {object|null} settings
 * @returns {string|null}
 */
export function rewardUnavailableMessage(kind, settings) {
  if (isRewardAvailable(kind, settings)) return null;
  const key = providerModule(kind);
  const m = MODULES[key] ?? { emoji: '', label: key };
  const label = kind === 'xp' ? 'XP' : 'Coin';
  if (!hasProvider(kind)) return `${label} rewards are not available on this bot (the ${m.emoji} ${m.label} module is not installed).`;
  return `${label} rewards need the **${m.emoji} ${m.label}** module. Enable it with \`/module enable ${key}\`.`;
}

// ── Display helpers ──────────────────────────────────────────────────────────────────────────────────────

/** The badge shown next to an achievement: its emoji, or the trigger's default. */
export function badgeOf(a) {
  return a.emoji ?? TRIGGER_EMOJI[a.trigger];
}

/** `Name` for single-tier achievements, `Name (Gold)` otherwise. */
export function achievementLabel(a, tierIndex) {
  return a.tiers.length === 1 ? a.name : `${a.name} (${TIER_NAMES[tierIndex]})`;
}

const roleText = (roleId, guild) => {
  const cache = guild?.roles?.cache;
  return cache && !cache.has(roleId) ? '@deleted-role ⚠️' : `<@&${roleId}>`;
};

/**
 * Human text for a reward list. Display only: a role missing from the cache is flagged, never removed.
 * @param {Array<{ type: string, roleId?: string, amount?: number }>} rewards
 * @param {import('discord.js').Guild|null} guild
 * @param {object|null} settings
 * @returns {string}
 */
export function formatRewards(rewards, guild, settings) {
  const parts = [];
  for (const r of rewards ?? []) {
    if (r.type === 'ROLE_ADD') parts.push(roleText(r.roleId, guild));
    else if (r.type === 'ROLE_REMOVE') parts.push(`−${roleText(r.roleId, guild)}`);
    else if (r.type === 'XP') parts.push(`+${formatNumber(r.amount)} XP`);
    else if (r.type === 'COINS') parts.push(`+${formatNumber(r.amount)} ${settings?.currencyEmoji ?? '🪙'} ${settings?.currencyName ?? 'coins'}`);
  }
  return clampText(parts.join(', '), 900);
}

/** 'on' | 'off' | 'ended' for a stored achievement row (or AchLite with deadlineMs). */
export function achievementStatus(a, nowMs = Date.now()) {
  if (a.enabled === false) return 'off';
  const deadline = a.deadlineMs ?? (a.deadline ? new Date(a.deadline).getTime() : null);
  return deadline != null && deadline < nowMs ? 'ended' : 'on';
}

/**
 * Variables for the announcement template and the almost-there DM (spec §1.3).
 * @param {{ achievement: object, tierIndex: number, progress: number, userId: string, displayName: string, rewardsText?: string }} o
 * @returns {Record<string, string>}
 */
export function buildAnnouncementVars({ achievement: a, tierIndex, progress, userId, displayName, rewardsText = '' }) {
  const tier = a.tiers[tierIndex] ?? a.tiers[a.tiers.length - 1];
  const idx = Math.min(Math.max(tierIndex, 0), TIER_NAMES.length - 1);
  return {
    player: `<@${userId}>`,
    'player.name': escapeMarkdown(String(displayName ?? '')),
    achievement: achievementLabel(a, idx),
    'achievement.name': a.name,
    'achievement.description': a.description ?? '',
    tier: TIER_NAMES[idx],
    'tier.emoji': TIER_EMOJIS[idx],
    goal: formatNumber(tier?.goal ?? 0),
    progress: formatNumber(progress),
    rewards: rewardsText ?? '',
  };
}

/** Sample values for the `announce message` preview. */
export function sampleVars(member) {
  return {
    player: `<@${member?.id ?? '0'}>`,
    'player.name': escapeMarkdown(String(member?.displayName ?? 'Player')),
    achievement: 'Chatterbox (Gold)',
    'achievement.name': 'Chatterbox',
    'achievement.description': 'Send messages in the server',
    tier: 'Gold',
    'tier.emoji': '🥇',
    goal: '2,500',
    progress: '2,500',
    rewards: '+100 XP',
  };
}

/**
 * Where an announcement goes. CHANNEL tries the configured channel then the source; CURRENT the other way
 * round; every other mode has no channel. Pure.
 * @param {string} mode
 * @param {{ source?: object|null, configured?: object|null }} channels
 * @param {(channel: object) => boolean} isUsable
 * @returns {object|null}
 */
export function pickAnnounceChannel(mode, { source = null, configured = null } = {}, isUsable = () => true) {
  let order;
  if (mode === 'CHANNEL') order = [configured, source];
  else if (mode === 'CURRENT') order = [source, configured];
  else return null;
  return order.find((ch) => ch && isUsable(ch)) ?? null;
}

// ── Progress maths ───────────────────────────────────────────────────────────────────────────────────────

/**
 * What a progress row means for its tiers: a tier to claim, or an "almost there" notice. Pure.
 * @param {Array<{ goal: number }>} tiers
 * @param {{ progress: number, tier: number, almostNotified: boolean }} row
 * @returns {{ advanceTo: number|null, almost: { tierIndex: number, goal: number }|null }}
 */
export function evaluateProgress(tiers, row) {
  const reached = tiers.reduce((n, t) => (row.progress >= t.goal ? n + 1 : n), 0);
  const current = Math.min(row.tier, tiers.length);
  if (reached > current) return { advanceTo: reached, almost: null };
  if (current >= tiers.length || row.almostNotified) return { advanceTo: null, almost: null };
  const goal = tiers[current].goal;
  const threshold = Math.ceil(goal * ALMOST_RATIO);
  if (threshold >= goal) return { advanceTo: null, almost: null }; // goals below 10 never get an almost-there
  return (row.progress >= threshold && row.progress < goal)
    ? { advanceTo: null, almost: { tierIndex: current, goal } }
    : { advanceTo: null, almost: null };
}

/**
 * True when this guildMemberUpdate is a boost starting (premiumSince going from unset to set). Pure.
 * @param {{ oldKnown?: boolean, oldMember?: object, newMember?: object }} ctx
 * @param {number} [now]
 */
export function isBoostStart(ctx, now = Date.now()) {
  const n = ctx.newMember?.premiumSinceTimestamp ?? null;
  if (!n) return false;
  if (ctx.oldKnown) return !ctx.oldMember?.premiumSinceTimestamp;
  return now - n >= 0 && now - n <= BOOST_FRESH_MS; // the old member was partial: only a fresh premiumSince counts
}

/** Achievements that still count: they have tiers and their deadline has not passed. */
export function activeAchievements(list, nowMs) {
  return (list ?? []).filter((a) => a.tiers.length > 0 && (a.deadlineMs == null || nowMs <= a.deadlineMs));
}

/**
 * What /achievements shows for one achievement. With a row: its values. Without one: the past progress the member
 * would be seeded with (only when trackPastProgress), and whether that already meets a goal.
 * @param {{ trigger: string, tiers: object[], trackPastProgress: boolean }} a
 * @param {{ progress: number, tier: number }|null|undefined} row
 * @param {{ messageCount?: number, level?: number }|null|undefined} ul
 * @param {{ premiumSinceTimestamp?: number|null }|null|undefined} member
 * @returns {{ progress: number, tier: number, pending: boolean }}
 */
export function displayProgress(a, row, ul, member) {
  const tiers = normalizeTiers(a.tiers);
  if (row) return { progress: row.progress, tier: Math.min(row.tier, tiers.length), pending: false };
  let seed = 0;
  if (a.trackPastProgress) {
    if (a.trigger === 'MESSAGES') seed = ul?.messageCount ?? 0;
    else if (a.trigger === 'LEVEL') seed = ul?.level ?? 0;
    else if (a.trigger === 'BOOST') seed = member?.premiumSinceTimestamp ? 1 : 0;
  }
  const seedReached = tiers.reduce((n, t) => (seed >= t.goal ? n + 1 : n), 0);
  return { progress: seed, tier: 0, pending: seedReached > 0 };
}

const unitOf = (trigger, n) => TRIGGER_UNITS[trigger][n === 1 ? 0 : 1];

/** `10 messages`, `1 boost`, or `level 10` for the LEVEL trigger. */
export function goalText(trigger, goal) {
  return trigger === 'LEVEL' ? `level ${formatNumber(goal)}` : `${formatNumber(goal)} ${unitOf(trigger, goal)}`;
}

/**
 * Up to 3 lines for one achievement in /achievements.
 * @param {object} a AchLite (tiers normalised)
 * @param {{ progress: number, tier: number, pending?: boolean, enabled?: boolean }} view
 * @param {number} nowMs
 * @returns {string}
 */
export function formatMemberEntry(a, view, nowMs) {
  const t = a.tiers;
  const u = view.tier;
  const p = view.progress;
  let line1 = `${badgeOf(a)} **${a.name}**`;
  if (u > 0) line1 += t.length === 1 ? ' — ✅ Unlocked' : ` — ${TIER_EMOJIS[u - 1]} ${TIER_NAMES[u - 1]}`;
  if (view.enabled === false) line1 += ' (retired)';

  let line2;
  if (u >= t.length) {
    line2 = a.trigger === 'LEVEL' ? `✅ Completed · level ${formatNumber(p)}` : `✅ Completed · ${formatNumber(p)} ${unitOf(a.trigger, p)}`;
  } else {
    const g = t[u].goal;
    line2 = `${progressBar(Math.min(p / g, 1), 10)} ${formatNumber(Math.min(p, g))}/${formatNumber(g)}`;
    if (t.length > 1) line2 += ` → ${TIER_EMOJIS[u]} ${TIER_NAMES[u]}`;
    if (view.pending && p >= g) line2 += ' · unlocks on your next activity';
  }
  if (a.deadlineMs != null) {
    line2 += a.deadlineMs < nowMs ? ' · ⌛ ended' : ` · ⏳ ends ${discordTimestamp(new Date(a.deadlineMs), 'R')}`;
  }
  const lines = [line1, line2];
  if (a.description) lines.push(`*${clampText(a.description, 100)}*`);
  return lines.join('\n');
}

// ── Index (spec §6.1) ────────────────────────────────────────────────────────────────────────────────────

/** @returns {{ byTrigger: Record<string, object[]> }} */
export function buildIndex(items) {
  return { byTrigger: Object.fromEntries(TRIGGERS.map((t) => [t, (items ?? []).filter((a) => a.trigger === t)])) };
}

/** A DB row as primitives only (what the cache stores; deadline as epoch ms). */
export function toLite(row) {
  return {
    id: row.id,
    name: row.name,
    description: row.description ?? null,
    emoji: row.emoji ?? null,
    trigger: row.trigger,
    tiers: normalizeTiers(row.tiers),
    trackPastProgress: row.trackPastProgress !== false,
    deadlineMs: row.deadline ? new Date(row.deadline).getTime() : null,
  };
}

function memoSet(guildId, byTrigger) {
  indexMemo.delete(guildId);
  const entry = { at: Date.now(), byTrigger };
  indexMemo.set(guildId, entry);
  while (indexMemo.size > MEMO_MAX) indexMemo.delete(indexMemo.keys().next().value);
  return entry;
}

async function loadItemsFromDb(guildId) {
  const rows = await prisma.achievement.findMany({
    where: { guildId, enabled: true },
    orderBy: [{ createdAt: 'asc' }, { id: 'asc' }],
  });
  return rows.map(toLite).filter((a) => a.tiers.length > 0);
}

async function loadIndex(guildId) {
  const startedAt = Date.now();
  const key = achievementsKey(guildId);
  // A slow reader must never replace a fresher memo (a config write may have refreshed it meanwhile).
  const remember = (byTrigger) => {
    const cur = indexMemo.get(guildId);
    return cur && cur.at >= startedAt ? cur : memoSet(guildId, byTrigger);
  };
  // Redis down or a miss: fail OPEN to Postgres (the cache is only a cache).
  const c = await cache.get(key);
  if (c?.v === 1 && c.guildId === guildId && Array.isArray(c.items)) return remember(buildIndex(c.items).byTrigger);
  // Postgres down: fail CLOSED. The caller gets null and skips the event, so nothing is counted against an unknown config.
  try {
    const items = await loadItemsFromDb(guildId);
    // NX: a stale reader can never overwrite what a writer put there (refreshAchievementCache uses a plain SET).
    await cache.setNX(key, { v: 1, guildId, items }, ACH_CACHE_TTL_S);
    return remember(buildIndex(items).byTrigger);
  } catch (err) {
    warnOnce(guildId, 'db', 'achievements: index load failed', err);
    return null;
  }
}

/**
 * The enabled achievements of a guild grouped by trigger. Never throws; null = Postgres is unreachable.
 * @param {string} guildId
 * @returns {Promise<{ byTrigger: Record<string, object[]> }|null>}
 */
export async function getAchievementIndex(guildId) {
  try {
    const m = indexMemo.get(guildId);
    if (m && Date.now() - m.at < ACH_MEMO_TTL_MS) return m;
    const pending = indexLoading.get(guildId);
    if (pending) return await pending;
    const promise = loadIndex(guildId);
    indexLoading.set(guildId, promise);
    const clear = () => { if (indexLoading.get(guildId) === promise) indexLoading.delete(guildId); };
    promise.then(clear, clear);
    return await promise;
  } catch (err) {
    warnOnce(guildId, 'db', 'achievements: index load failed', err);
    return null;
  }
}

/** Rebuilds the memo and the Redis copy after a config write. Never throws. */
export async function refreshAchievementCache(guildId) {
  try {
    indexMemo.delete(guildId);
    const items = await loadItemsFromDb(guildId);
    await cache.set(achievementsKey(guildId), { v: 1, guildId, items }, ACH_CACHE_TTL_S); // a plain SET overwrites
    memoSet(guildId, buildIndex(items).byTrigger);
  } catch (err) {
    warnOnce(guildId, 'db', 'achievements: cache refresh failed', err);
    await cache.del(achievementsKey(guildId));
  }
}

/**
 * Sync hint for the hook filters: false only when the memo is fresh and none of `triggers` has an achievement.
 * A missing or stale memo says "maybe", so the hook loads the index.
 */
export function mayHaveTriggers(guildId, triggers) {
  const m = indexMemo.get(guildId);
  if (!m || Date.now() - m.at >= ACH_MEMO_TTL_MS) return true;
  return triggers.some((t) => (m.byTrigger[t]?.length ?? 0) > 0);
}

// ── Raw SQL (spec §6.3; builders are exported as RAW_SQL) ────────────────────────────────────────────────

function updateSql({ userId, ids, inc, floor }) {
  return Prisma.sql`
    UPDATE "AchievementProgress"
    SET "progress" = LEAST(GREATEST("progress"::bigint + ${inc}::bigint, ${floor}::bigint), ${PROGRESS_MAX}::bigint)::int,
        "updatedAt" = ${SQL_NOW}
    WHERE "userId" = ${userId}::text AND "achievementId" = ANY(${ids}::text[])
    RETURNING "id", "achievementId", "progress", "tier", "almostNotified"`;
}

function insertSql({ guildId, userId, newIds, aids, inits }) {
  return Prisma.sql`
    INSERT INTO "AchievementProgress" ("id", "guildId", "achievementId", "userId", "progress", "createdAt", "updatedAt")
    SELECT t.id, ${guildId}::text, t.aid, ${userId}::text, LEAST(t.init::bigint, ${PROGRESS_MAX}::bigint)::int, ${SQL_NOW}, ${SQL_NOW}
    FROM unnest(${newIds}::text[], ${aids}::text[], ${inits}::int[]) AS t(id, aid, init)
    ON CONFLICT ("achievementId", "userId") DO NOTHING
    RETURNING "id", "achievementId", "progress", "tier", "almostNotified"`;
}

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'achievements.progressUpdate', build: () => updateSql({ userId: '0', ids: ['sample'], inc: 1, floor: 0 }) },
  // Empty arrays: the sample inserts nothing, so a generic runner needs no GuildSettings or Achievement rows (foreign keys).
  { name: 'achievements.progressInsert', build: () => insertSql({ guildId: '0', userId: '0', newIds: [], aids: [], inits: [] }) },
]);

async function readUserLevel(guildId, userId) {
  return prisma.userLevel.findUnique({
    where: { guildId_userId: { guildId, userId } },
    select: { messageCount: true, level: true },
  });
}

// ── Recording progress (spec §6.3) ───────────────────────────────────────────────────────────────────────

/**
 * Adds `inc` (count triggers) or raises progress to `floor` (LEVEL) for every listed achievement of one member,
 * creates the missing rows, then evaluates each row for unlocks. Never throws; null on a database error.
 * @returns {Promise<object[]|null>} the progress rows after the write
 */
export async function recordProgress({ ctx, settings, userId, member = null, trigger, achievements, inc = 0, floor = 0, channel = null }) {
  const guildId = ctx.guildId;
  try {
    const list = achievements ?? [];
    if (!list.length) return [];
    const byId = new Map(list.map((a) => [a.id, a]));
    const incN = toInt(inc, 0, PROGRESS_MAX);
    const floorN = toInt(floor, 0, PROGRESS_MAX);

    // Phase 1: the common case, one atomic statement (the row lock makes concurrent increments safe).
    const rows = await prisma.$queryRaw(updateSql({ userId, ids: list.map((a) => a.id), inc: incN, floor: floorN }));
    const have = new Set(rows.map((r) => r.achievementId));
    const missing = list.filter((a) => !have.has(a.id));

    // Phase 2: first event of this member for these achievements.
    if (missing.length) {
      await ensureGuildSettings(guildId);
      let ul = null;
      if (trigger === 'MESSAGES' && missing.some((a) => a.trackPastProgress)) {
        try {
          ul = await readUserLevel(guildId, userId);
        } catch {
          ul = null; // a read error means a seed of 0
        }
      }
      const init = (a) => {
        // The first of the incN messages seeds from the history (the levels hook may already have counted it: max, not
        // plus); the buffered rest add on top, as if each had been written on its own.
        if (trigger === 'MESSAGES') return a.trackPastProgress ? Math.max(ul?.messageCount ?? 0, 1) + Math.max(incN - 1, 0) : incN;
        if (trigger === 'LEVEL') return floorN;
        return incN;
      };
      const inserted = await prisma.$queryRaw(insertSql({
        guildId,
        userId,
        newIds: missing.map(() => randomUUID()),
        aids: missing.map((a) => a.id),
        inits: missing.map((a) => toInt(init(a), 0, PROGRESS_MAX)),
      }));
      rows.push(...inserted);
      // Phase 3: a concurrent dispatch created some of those rows first; add our share to them.
      if (inserted.length < missing.length) {
        const created = new Set(inserted.map((r) => r.achievementId));
        const conflicted = missing.filter((a) => !created.has(a.id));
        const again = await prisma.$queryRaw(updateSql({ userId, ids: conflicted.map((a) => a.id), inc: incN, floor: floorN }));
        rows.push(...again);
      }
    }

    if (trigger === 'LEVEL' || trigger === 'BOOST') for (const r of rows) markSeeded(r.achievementId, userId);
    for (const row of rows) {
      const a = byId.get(row.achievementId);
      if (!a) continue;
      try {
        await handleRow({ ctx, settings, achievement: a, row, userId, member, channel });
      } catch (err) {
        warnOnce(guildId, 'progress', 'achievements: row evaluation failed', err);
      }
    }
    return rows;
  } catch (err) {
    warnOnce(guildId, 'progress', 'achievements: progress update failed', err);
    // An achievement deleted mid-flight raises an FK violation in Phase 2; drop the stale index.
    if (err?.code === 'P2003' || /foreign key/i.test(String(err?.message))) await refreshAchievementCache(guildId);
    return null;
  }
}

/**
 * LEVEL and BOOST have no event for the past, so a member's first message seeds them (spec §6.5): one UserLevel read
 * and at most one INSERT per member per process lifetime per achievement; after that a Map lookup. Never throws.
 */
export async function catchUpSeeds({ ctx, settings, idx, userId, member = null, channel = null }) {
  const guildId = ctx.guildId;
  try {
    const now = Date.now();
    let cands = [...activeAchievements(idx.byTrigger.LEVEL, now), ...activeAchievements(idx.byTrigger.BOOST, now)]
      .filter((a) => a.trackPastProgress && !seeded.has(`${a.id}:${userId}`));
    if (!cands.length) return;

    let level = 0;
    if (cands.some((a) => a.trigger === 'LEVEL')) {
      try {
        level = (await readUserLevel(guildId, userId))?.level ?? 0;
      } catch (err) {
        warnOnce(guildId, 'progress', 'achievements: catch-up read failed', err);
        return; // not marked, so the next message tries again
      }
    }
    // Without the member (partial message author) a boost cannot be read: leave those unmarked for a later message.
    if (!member) cands = cands.filter((a) => a.trigger !== 'BOOST');
    if (!cands.length) return;
    const boosting = Boolean(member?.premiumSinceTimestamp);
    const initOf = (a) => (a.trigger === 'LEVEL' ? level : (boosting ? 1 : 0));
    const toInsert = cands.filter((a) => initOf(a) >= 1);

    if (toInsert.length) {
      await ensureGuildSettings(guildId);
      const inserted = await prisma.$queryRaw(insertSql({
        guildId,
        userId,
        newIds: toInsert.map(() => randomUUID()),
        aids: toInsert.map((a) => a.id),
        inits: toInsert.map((a) => toInt(initOf(a), 0, PROGRESS_MAX)),
      }));
      const byId = new Map(toInsert.map((a) => [a.id, a]));
      // Rows that already existed return nothing, so their progress (including a reset) is never touched.
      for (const row of inserted) {
        const a = byId.get(row.achievementId);
        if (!a) continue;
        try {
          await handleRow({ ctx, settings, achievement: a, row, userId, member, channel });
        } catch (err) {
          warnOnce(guildId, 'progress', 'achievements: row evaluation failed', err);
        }
      }
    }
    for (const a of cands) markSeeded(a.id, userId);
  } catch (err) {
    warnOnce(guildId, 'progress', 'achievements: catch-up failed', err);
  }
}

// ── Claiming unlocks (spec §6.6) ─────────────────────────────────────────────────────────────────────────

/**
 * Evaluates one progress row. An unlock is claimed with a compare-and-set on `tier` BEFORE anything is paid
 * (so each tier pays at most once); the payout and announcement are started without awaiting.
 */
export async function handleRow({ ctx, settings, achievement: a, row, userId, member = null, channel = null }) {
  const e = evaluateProgress(a.tiers, row);
  if (e.advanceTo) {
    const { count } = await prisma.achievementProgress.updateMany({
      where: { id: row.id, tier: row.tier },
      data: { tier: e.advanceTo, almostNotified: false, completedAt: e.advanceTo >= a.tiers.length ? new Date() : null },
    });
    if (count === 1) {
      celebrate({
        ctx, settings, achievement: a, userId, member,
        fromTier: Math.min(row.tier, a.tiers.length), toTier: e.advanceTo, progress: row.progress, channel,
      }).catch((err) => logger.error('achievements: celebrate failed', err));
    }
    return;
  }
  if (e.almost && settings?.achievementAnnounceMode === 'DM') {
    const { count } = await prisma.achievementProgress.updateMany({
      where: { id: row.id, tier: row.tier, almostNotified: false },
      data: { almostNotified: true },
    });
    if (count === 1) {
      sendAlmostThere({
        ctx, member, userId, achievement: a, tierIndex: e.almost.tierIndex, goal: e.almost.goal, progress: row.progress,
      }).catch((err) => logger.error('achievements: almost-there failed', err));
    }
  }
}

// ── Payout and announcement (spec §6.7 to §6.10) ─────────────────────────────────────────────────────────

/** Pays the rewards of the unlocked tiers, then announces. Never awaited by a hook. */
export async function celebrate({ ctx, settings, achievement: a, userId, member = null, fromTier, toTier, progress, channel = null }) {
  const guild = ctx.guild;
  const who = member ?? await guild.members.fetch(userId).catch(() => null);
  const rewardLines = await payTierRewards({ guild, member: who, userId, achievement: a, fromTier, toTier, channel, settings });
  if (!who) return; // the member left: XP and coins were granted; roles and the announcement are skipped
  await announceUnlock({
    guild, member: who, achievement: a, tierIndex: toTier - 1, progress,
    rewardsText: joinWithLimit(rewardLines, { max: 300 }), channel, settings,
  });
}

async function editRole(guild, member, roleId, add, a, tierIndex) {
  const role = guild.roles.cache.get(roleId);
  const chk = checkAssignableRole(guild, role, add ? {} : { allowDangerous: true });
  if (!chk.ok) {
    warnOnce(guild.id, 'role', `achievements: cannot ${add ? 'give' : 'take'} role ${roleId} (${chk.reason})`);
    return false;
  }
  if (member.roles.cache.has(roleId) === add) return false; // already held (add) or not held (remove)
  try {
    const reason = clampText(`Achievement: ${a.name} (${TIER_NAMES[tierIndex]})`, 512);
    // One role per call; the array form would PATCH the member's whole role list.
    if (add) await member.roles.add(roleId, reason);
    else await member.roles.remove(roleId, reason);
    return true;
  } catch (err) {
    const code = err?.code;
    if (code === 10011 || code === 10007) return false;
    if (code === 50013 || code === 50001) warnOnce(guild.id, 'perm', `achievements: missing permission to edit role ${roleId}`);
    else logger.warn(`achievements: role edit failed (${err?.message})`);
    return false;
  }
}

/**
 * Applies the rewards of tiers `fromTier` to `toTier - 1` in tier order and returns the summary lines. Never throws.
 * Payout is at most once (spec D9): a rejected role edit or a lost process is not retried.
 */
export async function payTierRewards({ guild, member, userId, achievement: a, fromTier, toTier, channel = null, settings = null }) {
  const lines = [];
  let xp = 0;
  let coins = 0;
  try {
    for (let i = fromTier; i < toTier; i++) {
      for (const r of a.tiers[i]?.rewards ?? []) {
        if (r.type === 'XP') xp += r.amount;
        else if (r.type === 'COINS') coins += r.amount;
        else if (!member) continue;
        else if (r.type === 'ROLE_ADD') {
          if (await editRole(guild, member, r.roleId, true, a, i)) lines.push(`<@&${r.roleId}>`);
        } else if (r.type === 'ROLE_REMOVE') {
          if (await editRole(guild, member, r.roleId, false, a, i)) lines.push(`−<@&${r.roleId}>`);
        }
      }
    }
    const reason = clampText(`Achievement: ${a.name} (${TIER_NAMES[Math.max(0, toTier - 1)]})`, 100);
    for (const [kind, sum] of [['xp', xp], ['coins', coins]]) {
      if (sum < 1) continue;
      const amount = Math.min(sum, 1_000_000_000);
      const r = await grant(kind, { guild, userId, amount, reason, member, channel });
      if (r.ok) {
        lines.push(kind === 'xp'
          ? `+${formatNumber(amount)} XP`
          : `+${formatNumber(amount)} ${settings?.currencyEmoji ?? '🪙'} ${settings?.currencyName ?? 'coins'}`);
      } else {
        warnOnce(guild.id, `reward-${kind}`, `achievements: ${kind} reward not paid (${r.reason})`);
      }
    }
  } catch (err) {
    logger.error('achievements: payout failed', err);
  }
  return lines;
}

const sendErrorCode = (err) => err?.code;

/** True when an announcement template pings the member ({player}, {mention}, {user}, {user.mention}). Same rule as level-ups. */
export function templateMentionsPlayer(template) {
  return /\{(player|mention|user|user\.mention)\}/i.test(String(template ?? ''));
}

/**
 * Announces an unlock according to `achievementAnnounceMode`. The configured text goes through the template
 * engine (overview rule 11); with `achievementUseEmbed` it is the embed description. Never throws.
 */
export async function announceUnlock({ guild, member, achievement: a, tierIndex, progress, rewardsText = '', channel = null, settings = null }) {
  try {
    const mode = settings?.achievementAnnounceMode ?? 'CURRENT';
    if (mode === 'OFF') return;
    const vars = buildAnnouncementVars({
      achievement: a, tierIndex, progress, userId: member.id, displayName: member.displayName, rewardsText,
    });
    const text = settings?.achievementMessage?.trim() || DEFAULT_ACHIEVEMENT_MESSAGE;
    const useEmbed = settings?.achievementUseEmbed === true;
    const color = Number.isInteger(settings?.embedColor) && settings.embedColor >= 0 && settings.embedColor <= 0xffffff
      ? settings.embedColor : undefined;
    // A mention inside an embed never notifies, so in embed mode a template that pings the member also puts
    // the mention in the message content (channel announcements only: a DM needs no ping). The ping is scoped to
    // this member by allowedMentions, never a role or @everyone.
    const pingInContent = useEmbed && mode !== 'DM' && templateMentionsPlayer(text);
    const out = renderTemplate(
      useEmbed ? { content: pingInContent ? `<@${member.id}>` : null, embed: { description: text } } : { content: text },
      { member, user: member.user, guild, channel, vars },
      { defaultColor: color, allowedMentions: { parse: [], users: [member.id] } },
    );
    const hasEmbed = Boolean(out?.embeds?.length);
    // Nothing visible rendered: never a bare ping (embed mode needs the embed, text mode needs its text).
    if (!out || (useEmbed ? !hasEmbed : !out.content?.trim())) return;

    if (mode === 'DM') {
      const footer = `-# Sent from **${escapeMarkdown(guild.name)}**`;
      const content = `${clampText(out.content ?? '', 2000 - footer.length - 1)}\n${footer}`.trim();
      try {
        await member.send({ content, ...(hasEmbed ? { embeds: out.embeds } : {}), allowedMentions: { parse: [] } });
      } catch (err) {
        if (sendErrorCode(err) !== 50007) logger.debug(`achievements: DM failed (${err?.message})`);
      }
      return;
    }

    const needs = useEmbed ? ['ViewChannel', 'SendMessages', 'EmbedLinks'] : ['ViewChannel', 'SendMessages'];
    const target = pickAnnounceChannel(
      mode,
      { source: channel, configured: guild.channels.cache.get(settings?.achievementChannelId) ?? null },
      (ch) => checkChannelUsable(ch, { needs }).ok,
    );
    if (!target) return; // spec D7: no guessing at a public channel
    try {
      await target.send({
        ...(out.content ? { content: out.content } : {}),
        ...(hasEmbed ? { embeds: out.embeds } : {}),
        allowedMentions: out.allowedMentions,
      });
    } catch (err) {
      const code = sendErrorCode(err);
      if (code === 50013 || code === 50001) warnOnce(guild.id, 'announce', `achievements: cannot announce in ${target.id} (missing permission)`);
      else if (code !== 10003) logger.warn(`achievements: announcement failed (${err?.message})`);
    }
  } catch (err) {
    logger.error('achievements: announcement failed', err);
  }
}

/** One DM per tier at 90 % of the next goal (only in DM mode). Never throws. */
export async function sendAlmostThere({ ctx, member = null, userId, achievement: a, tierIndex, goal, progress }) {
  try {
    const user = member?.user ?? await ctx.client?.users?.fetch(userId).catch(() => null);
    if (!user) return;
    const vars = {
      ...buildAnnouncementVars({
        achievement: a, tierIndex, progress, userId, displayName: member?.displayName ?? user.globalName ?? user.username, rewardsText: '',
      }),
      goal: formatNumber(goal),
    };
    const content = renderPlaceholders(ALMOST_THERE_MESSAGE, { member, user, guild: ctx.guild, vars }, { maxLength: 2000 });
    try {
      await user.send({ content, allowedMentions: { parse: [] } });
    } catch (err) {
      if (sendErrorCode(err) !== 50007) logger.debug(`achievements: almost-there DM failed (${err?.message})`);
    }
  } catch (err) {
    logger.error('achievements: almost-there failed', err);
  }
}

// ── Message buffer (write-behind) ──────────────────────────────────────────────────────────────────────────

/**
 * MESSAGES progress is summed in memory and written per member as one UPDATE (+n) once `delayMs` after their first waiting
 * message, or at once when `at` are waiting: a chatty member costs one statement every few seconds instead of one per
 * message. Counts are unchanged; unlocks and their announcements come up to `delayMs` late, and a crash loses at most
 * that window (bot.js flushes at shutdown). At most `maxMembers` members wait; beyond that the oldest is written at once.
 */
export const MESSAGE_FLUSH = Object.freeze({ delayMs: 5_000, at: 50, maxMembers: 10_000 });
let flushConfig = { ...MESSAGE_FLUSH };
/** @type {Map<string, object>} `${guildId}:${userId}` -> the waiting messages of one member */
const pendingMessages = new Map();
/** @type {Map<Promise<void>, string>} writes in progress -> their member key, so flushes and resets can wait for them */
const runningFlushes = new Map();
/** Set by the final flush at shutdown: later messages are written at once instead of waiting on a timer. */
let bufferStopped = false;

/** Test helper: changes the buffer. `delayMs: 0` writes every message at once, as before the buffer. */
export function __setMessageFlush(over = {}) {
  flushConfig = { ...MESSAGE_FLUSH, ...over };
}

function bufferMessage({ ctx, settings, userId, member, channel, achievements }) {
  const key = `${ctx.guildId}:${userId}`;
  const ids = achievements.map((a) => a.id);
  const sig = ids.join(',');
  let entry = pendingMessages.get(key);
  // Another set of active achievements (one created or deleted, or past its deadline): the waiting messages belong to the old set.
  if (entry && entry.sig !== sig) {
    flushMember(key);
    entry = null;
  }
  if (!entry) {
    while (pendingMessages.size >= flushConfig.maxMembers) flushMember(pendingMessages.keys().next().value);
    entry = { guildId: ctx.guildId, userId, sig, ids, inc: 0, timer: null };
    pendingMessages.set(key, entry);
    entry.timer = setTimeout(() => flushMember(key), flushConfig.delayMs);
    entry.timer.unref?.();
  }
  entry.inc++;
  // The newest message wins: an unlock is announced where the member wrote last.
  entry.ctx = { guildId: ctx.guildId, guild: ctx.guild, client: ctx.client };
  entry.settings = settings;
  entry.member = member ?? entry.member ?? null;
  entry.channel = channel ?? entry.channel ?? null;
  if (entry.inc >= flushConfig.at) flushMember(key);
}

/** Starts writing one member's waiting messages. Never rejects. */
function flushMember(key) {
  const entry = pendingMessages.get(key);
  if (!entry) return;
  pendingMessages.delete(key);
  clearTimeout(entry.timer);
  const run = writeMessages(entry)
    .catch((err) => warnOnce(entry.guildId, 'progress', 'achievements: buffered progress write failed', err))
    .finally(() => runningFlushes.delete(run));
  runningFlushes.set(run, key);
}

/** Writes one member's waiting messages and waits for every write of that member. Never rejects. */
async function settleMember(guildId, userId) {
  const key = `${guildId}:${userId}`;
  flushMember(key);
  await Promise.allSettled([...runningFlushes].filter(([, k]) => k === key).map(([run]) => run));
}

async function writeMessages(entry) {
  // The achievements as they are now (tier edits apply, a deleted one is dropped), limited to the ones these messages counted for.
  const idx = await getAchievementIndex(entry.guildId);
  if (!idx) return;
  const current = new Map((idx.byTrigger.MESSAGES ?? []).map((a) => [a.id, a]));
  const list = entry.ids.map((id) => current.get(id)).filter(Boolean);
  if (!list.length) return;
  await recordProgress({
    ctx: entry.ctx, settings: entry.settings, userId: entry.userId, member: entry.member,
    trigger: 'MESSAGES', achievements: list, inc: entry.inc, channel: entry.channel,
  });
}

/**
 * Writes every waiting message count now and waits for all writes. Never rejects. `final` (bot.js, at shutdown): later
 * messages are written at once, since no timer would fire before the process exits.
 */
export async function flushMessageCounts({ final = false } = {}) {
  if (final) bufferStopped = true;
  for (const key of [...pendingMessages.keys()]) flushMember(key);
  await Promise.allSettled([...runningFlushes.keys()]);
}

// ── Hook filters and runs (spec §5, §6.2) ────────────────────────────────────────────────────────────────

export const messageFilter = (ctx) => !!ctx.guildId && !ctx.message?.author?.bot && mayHaveTriggers(ctx.guildId, MESSAGE_HOOK_TRIGGERS);
export const reactionFilter = (ctx) => !!ctx.guildId && !ctx.user?.bot && mayHaveTriggers(ctx.guildId, ['REACTIONS']);
export const memberUpdateFilter = (ctx) => !ctx.newMember?.user?.bot && isBoostStart(ctx) && mayHaveTriggers(ctx.guildId, ['BOOST']);
export const levelUpFilter = (ctx) => !!ctx.guildId && mayHaveTriggers(ctx.guildId, ['LEVEL']);
export const purchaseFilter = (ctx) => !!ctx.guildId && mayHaveTriggers(ctx.guildId, ['PURCHASES']);

export async function onMessageCreate(ctx) {
  const settings = await ctx.getSettings(); // memoised by the runner; non-null because the module gate passed
  const idx = await getAchievementIndex(ctx.guildId);
  if (!idx) return;
  const userId = ctx.message.author.id;
  const member = ctx.member ?? null;
  const channel = ctx.channel ?? null;
  const msgs = activeAchievements(idx.byTrigger.MESSAGES, Date.now());
  if (msgs.length) {
    if (flushConfig.delayMs > 0 && !bufferStopped) bufferMessage({ ctx, settings, userId, member, channel, achievements: msgs });
    else await recordProgress({ ctx, settings, userId, member, trigger: 'MESSAGES', achievements: msgs, inc: 1, channel });
  }
  await catchUpSeeds({ ctx, settings, idx, userId, member, channel });
}

export async function onReactionAdd(ctx) {
  const settings = await ctx.getSettings();
  const idx = await getAchievementIndex(ctx.guildId);
  if (!idx) return;
  const list = activeAchievements(idx.byTrigger.REACTIONS, Date.now());
  if (!list.length) return;
  // Each (message, emoji) pair counts once per member per 24 h (spec D6). Redis down (null) does NOT count:
  // fail closed, consistent with the XP cooldown, so an outage can never be farmed.
  const ok = await cache.setNX(`ach:react:${ctx.guildId}:${ctx.user.id}:${ctx.message.id}:${ctx.emojiKey}`, 1, REACTION_DEDUPE_TTL_S);
  if (ok !== true) return;
  const channel = ctx.guild.channels.cache.get(ctx.message.channelId) ?? null;
  // No member fetch here: it happens only on an unlock.
  await recordProgress({ ctx, settings, userId: ctx.user.id, member: null, trigger: 'REACTIONS', achievements: list, inc: 1, channel });
}

export async function onMemberUpdate(ctx) {
  const settings = await ctx.getSettings();
  const idx = await getAchievementIndex(ctx.guildId);
  if (!idx) return;
  const member = ctx.newMember;
  const list = activeAchievements(idx.byTrigger.BOOST, Date.now());
  if (!list.length) return;
  // false = duplicate event. null (Redis down): count only when the transition is certain (the old member was
  // cached); the 60 s heuristic path fails closed.
  const r = await cache.setNX(`ach:boost:${ctx.guildId}:${member.id}:${member.premiumSinceTimestamp}`, 1, BOOST_DEDUPE_TTL_S);
  if (r === false) return;
  if (r === null && !ctx.oldKnown) return;
  await recordProgress({ ctx, settings, userId: member.id, member, trigger: 'BOOST', achievements: list, inc: 1, channel: null });
}

export async function onLevelUp(ctx) {
  const settings = await ctx.getSettings();
  const idx = await getAchievementIndex(ctx.guildId);
  if (!idx) return;
  const userId = ctx.userId ?? ctx.member?.id;
  const newLevel = Number(ctx.newLevel);
  if (!userId || !Number.isInteger(newLevel) || newLevel < 1) return;
  if (ctx.member?.user?.bot) return;
  const list = activeAchievements(idx.byTrigger.LEVEL, Date.now());
  if (!list.length) return;
  // ctx.member can be null (the member left, or a bulk role give); everything below works from the id.
  await recordProgress({
    ctx, settings, userId, member: ctx.member ?? null, trigger: 'LEVEL', achievements: list,
    inc: 0, floor: Math.min(newLevel, PROGRESS_MAX), channel: ctx.channel ?? null,
  });
}

export async function onPurchase(ctx) {
  const settings = await ctx.getSettings();
  const idx = await getAchievementIndex(ctx.guildId);
  if (!idx) return;
  const userId = ctx.userId ?? ctx.member?.id;
  if (!userId) return;
  const list = activeAchievements(idx.byTrigger.PURCHASES, Date.now());
  if (!list.length) return;
  const q = Number.isInteger(ctx.quantity) && ctx.quantity > 0 ? Math.min(ctx.quantity, MAX_PURCHASE_DELTA) : 1;
  await recordProgress({
    ctx, settings, userId, member: ctx.member ?? null, trigger: 'PURCHASES', achievements: list, inc: q, channel: ctx.channel ?? null,
  });
}

// ── Data functions for the commands (spec §6.11) ─────────────────────────────────────────────────────────

/** @returns {Promise<object[]>} every achievement of the guild (enabled or not), oldest first */
export function listAchievements(guildId) {
  return prisma.achievement.findMany({ where: { guildId }, orderBy: [{ createdAt: 'asc' }, { id: 'asc' }] });
}

/** Resolves an autocomplete value (an id) or a typed name. Throws on a DB error. */
export async function resolveAchievement(guildId, input) {
  const s = String(input ?? '').trim();
  if (!s) return null;
  if (/^[a-z0-9]{20,40}$/i.test(s)) {
    const byId = await prisma.achievement.findFirst({ where: { id: s, guildId } });
    if (byId) return byId;
  }
  return prisma.achievement.findFirst({ where: { guildId, name: { equals: s, mode: 'insensitive' } } });
}

/** Case-insensitive duplicate check. Throws on a DB error. */
export async function nameTaken(guildId, name, exceptId = null) {
  const hit = await prisma.achievement.findFirst({
    where: { guildId, name: { equals: name, mode: 'insensitive' }, ...(exceptId ? { id: { not: exceptId } } : {}) },
    select: { id: true },
  });
  return Boolean(hit);
}

/** Autocomplete rows, filtered in the database. [] on error. */
export async function searchAchievements(guildId, focused) {
  try {
    return await prisma.achievement.findMany({
      where: { guildId, name: { contains: String(focused ?? ''), mode: 'insensitive' } },
      orderBy: { name: 'asc' },
      take: 25,
      select: { id: true, name: true, emoji: true, trigger: true, enabled: true },
    });
  } catch {
    return [];
  }
}

/**
 * Creates an achievement after the cap and the name check. Call inside withConfigLock, then refresh the cache.
 * @returns {Promise<{ ok: true, achievement: object } | { ok: false, code: 'limit'|'name-taken' }>}
 * @throws on DB failure
 */
export async function createAchievement(guildId, { name, description = null, emoji = null, trigger, trackPastProgress = true, deadline = null, tiers }) {
  const count = await prisma.achievement.count({ where: { guildId } });
  if (count >= LIMITS.ACHIEVEMENTS) return { ok: false, code: 'limit' };
  if (await nameTaken(guildId, name)) return { ok: false, code: 'name-taken' };
  await ensureGuildSettings(guildId);
  try {
    const achievement = await prisma.achievement.create({
      data: { guildId, name, description, emoji, trigger, trackPastProgress, deadline, tiers: serializeTiers(tiers) },
    });
    return { ok: true, achievement };
  } catch (err) {
    if (err?.code === 'P2002') return { ok: false, code: 'name-taken' };
    throw err;
  }
}

/**
 * Updates scalar fields of an achievement (never the trigger, D12). Call inside withConfigLock, then refresh.
 * @returns {Promise<{ ok: true } | { ok: false, code: 'not-found'|'name-taken' }>}
 * @throws on DB failure
 */
export async function updateAchievement(guildId, id, data) {
  try {
    const { count } = await prisma.achievement.updateMany({ where: { id, guildId }, data });
    return count === 0 ? { ok: false, code: 'not-found' } : { ok: true };
  } catch (err) {
    if (err?.code === 'P2002') return { ok: false, code: 'name-taken' };
    throw err;
  }
}

/** Deletes an achievement; the cascade removes its progress rows. @returns {Promise<number>} rows deleted */
export async function deleteAchievement(guildId, id) {
  const { count } = await prisma.achievement.deleteMany({ where: { id, guildId } });
  return count;
}

/**
 * Everything /achievements needs for one member in three parallel reads. Null on a database error.
 * @returns {Promise<{ entries: Array<{ a: object, enabled: boolean, row: object|null, display: object }> }|null>}
 */
export async function loadMemberView(guildId, userId, member) {
  try {
    const [rows, progress, ul] = await Promise.all([
      listAchievements(guildId),
      prisma.achievementProgress.findMany({ where: { guildId, userId } }),
      readUserLevel(guildId, userId),
    ]);
    const byAch = new Map(progress.map((r) => [r.achievementId, r]));
    const entries = [];
    for (const row of rows) {
      const p = byAch.get(row.id) ?? null;
      if (!row.enabled && !(p && p.tier >= 1)) continue; // disabled achievements show only when something was unlocked
      const a = toLite(row);
      if (!a.tiers.length) continue;
      entries.push({ a, enabled: row.enabled, row: p, display: displayProgress(a, p, ul, member) });
    }
    return { entries };
  } catch (err) {
    logger.warn('achievements: member view failed', err);
    return null;
  }
}

/**
 * Members per tier for /achievement-config info. `perTier[i]` counts rows that unlocked tier i (tier >= i + 1).
 * @throws on DB failure
 */
export async function getTierStats(achievementId, tierCount) {
  const groups = await prisma.achievementProgress.groupBy({ by: ['tier'], where: { achievementId }, _count: { _all: true } });
  const perTier = Array.from({ length: tierCount }, (_, i) => groups.reduce((n, g) => (g.tier >= i + 1 ? n + g._count._all : n), 0));
  const total = groups.reduce((n, g) => n + g._count._all, 0);
  return { perTier, total };
}

/**
 * Zeroes a member's progress (spec D8): rows are zeroed, never deleted, because a deleted row would be re-seeded
 * from history on the next message and pay the rewards again. Rewards already given are not taken back.
 * @throws on DB failure
 */
export async function resetMemberProgress(guildId, userId, achievementIds) {
  const ids = [...new Set(achievementIds ?? [])];
  if (!ids.length) return;
  // Messages still waiting in the buffer were sent before the reset: write them first, so they cannot land on the reset row.
  await settleMember(guildId, userId);
  await ensureGuildSettings(guildId);
  await prisma.$transaction([
    prisma.achievementProgress.updateMany({
      where: { guildId, userId, achievementId: { in: ids } },
      data: { progress: 0, tier: 0, almostNotified: false, completedAt: null },
    }),
    prisma.achievementProgress.createMany({
      data: ids.map((achievementId) => ({ guildId, userId, achievementId })),
      skipDuplicates: true,
    }),
  ]);
  for (const id of ids) markSeeded(id, userId);
}

/**
 * Saves new tiers together with the matching progress fix-ups in one transaction. Tiers are never revoked: a raised
 * goal leaves members who already unlocked it as they are; a lowered goal unlocks on their next activity.
 * @param {string} achievementId
 * @param {object[]} nextTiers
 * @param {{ kind: 'add'|'edit'|'remove', index?: number, goalChanged?: boolean }} change
 * @throws on DB failure
 */
export async function applyTierChange(achievementId, nextTiers, { kind, index = 0, goalChanged = false } = {}) {
  const k = index;
  const ops = [prisma.achievement.update({ where: { id: achievementId }, data: { tiers: serializeTiers(nextTiers) } })];
  if (kind === 'add') {
    // Progress resumes toward the new top tier.
    ops.push(prisma.achievementProgress.updateMany({ where: { achievementId, completedAt: { not: null } }, data: { completedAt: null } }));
  } else if (kind === 'remove') {
    const n = nextTiers.length;
    ops.push(
      // Members who had passed the removed tier lose one count (remapTierAfterRemoval).
      prisma.achievementProgress.updateMany({ where: { achievementId, tier: { gt: k } }, data: { tier: { decrement: 1 } } }),
      prisma.achievementProgress.updateMany({ where: { achievementId, tier: { gte: n }, completedAt: null }, data: { completedAt: new Date() } }),
      prisma.achievementProgress.updateMany({ where: { achievementId, tier: k }, data: { almostNotified: false } }),
    );
  } else if (kind === 'edit' && goalChanged) {
    ops.push(prisma.achievementProgress.updateMany({ where: { achievementId, tier: k }, data: { almostNotified: false } }));
  }
  await prisma.$transaction(ops);
}
