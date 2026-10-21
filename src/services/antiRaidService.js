// Anti-Raid: spots a join flood, runs a lockdown (invites paused, @everyone's send permissions taken, new joiners
// kicked or banned) and lifts it again. State of a lockdown is the AntiRaidState row (one per guild), so it survives
// restarts; the join windows and a short memo of "is a lockdown on" are in memory per shard (a guild lives on one).
// What happens to a joining member is in src/plugins/antiraid.js; /antiraid is src/commands/admin/antiraid.js.
//
// Chat lock: only the SEND_MASK bits (never View Channels, that is /captcha lock) are taken from @everyone, and only the
// ones the bot also holds through a role of its own (a bot without Administrator may only grant what it holds, and once
// @everyone lacks a bit the bot has it only via its roles); any other send bit stays and is named in the alert. The bits
// taken are saved in AntiRaidState.priorEveryoneSend BEFORE the role is edited, and ending ORs exactly those bits back
// into whatever the role has then, so a crash or a second call can never lose or invent a permission.
import { ActionRowBuilder, ButtonBuilder, ButtonStyle, PermissionFlagsBits, PermissionsBitField } from 'discord.js';
import prisma from '../database/client.js';
import { COLORS } from '../config/constants.js';
import { buildCustomId } from '../config/customIds.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';
import { cancelJobs, scheduleJob } from './scheduler.js';
import { MOD_SOURCES, executeModAction } from './moderationService.js';
import { createKeyedMutex, createSemaphore, createThrottle } from '../utils/async.js';
import { checkChannelUsable, missingBotPermissions, permissionNames } from '../utils/interactionHelpers.js';
import { neutralEmbed } from '../utils/embedBuilder.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import logger from '../utils/logger.js';

const MINUTE_MS = 60_000;
const DAY_MS = 86_400_000;

export const RAID = Object.freeze({
  // Ranges of the settings (the same as the dashboard fields).
  THRESHOLD: Object.freeze({ min: 3, max: 100 }),
  SECONDS: Object.freeze({ min: 3, max: 300 }),
  MINUTES: Object.freeze({ min: 5, max: 1440 }),
  AGE_DAYS: Object.freeze({ min: 0, max: 365 }),
  DEFAULT_MINUTES: 30,
  REASON_MAX: 200,
  // A "no lockdown" answer is trusted this long before the table is asked again.
  NONE_MEMO_MS: 30_000,
  // New-account alerts: at most one per guild per interval; the ones in between are counted into the next.
  YOUNG_ALERT_EVERY_MS: 60_000,
  // Kicks and bans of one raid wave run this many at a time.
  ACTION_CONCURRENCY: 4,
  // Messages of a banned raider from the last hour go with the ban.
  BAN_DELETE_SECONDS: 3600,
  // Discord pauses invites for at most 24 hours; stay a minute under.
  INVITE_CAP_MS: DAY_MS - MINUTE_MS,
  // A pause ending this close to AntiRaidState.invitesPausedUntil (the end we set) is the one we set.
  INVITE_MATCH_MS: 5000,
  // A job that fires this much before lockedUntil is not due yet.
  DUE_SLACK_MS: 5000,
  UNLOCK_JOB: 'antiraid.unlock',
  AUDIT_REASON_MAX: 512,
});

/** The @everyone permissions a chat lock takes (View Channels is never one of them). */
export const SEND_MASK = PermissionFlagsBits.SendMessages | PermissionFlagsBits.SendMessagesInThreads
  | PermissionFlagsBits.CreatePublicThreads | PermissionFlagsBits.CreatePrivateThreads;

export const ACTION_LABEL = Object.freeze({ ALERT: 'Only alert the moderators', KICK: 'Kick them', BAN: 'Ban them' });
const ACTION_RANK = Object.freeze({ ALERT: 0, KICK: 1, BAN: 2 });
const ALERT_GONE_CODES = new Set([10003, 50001, 50013]);

const mutex = createKeyedMutex();
const actionSlots = createSemaphore(RAID.ACTION_CONCURRENCY);
const youngThrottle = createThrottle(RAID.YOUNG_ALERT_EVERY_MS);
const noAlertThrottle = createThrottle(10 * MINUTE_MS);

/** @type {Map<string, number[]>} guildId -> join times inside the detection window */
const windows = new Map();
/** @type {Map<string, { active: boolean, until: number, at: number }>} */
const memo = new Map();
/** @type {Map<string, { kicked: number, banned: number, failed: number, flagged: number }>} */
const tallies = new Map();
/** @type {Map<string, { count: number, latest: object }>} new accounts seen since the last alert */
const youngPending = new Map();

/** Clears every in-memory window, memo and tally (tests). */
export function resetMemory() {
  windows.clear();
  memo.clear();
  tallies.clear();
  youngPending.clear();
}

const DEPS = { scheduleJob, cancelJobs, executeModAction, ensureGuildSettings };

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** An integer inside `range`, or `fallback` for anything else. */
export function clampInt(value, range, fallback) {
  const n = Number(value);
  if (!Number.isFinite(n)) return fallback;
  return Math.min(range.max, Math.max(range.min, Math.trunc(n)));
}

/** The harsher of two actions (BAN > KICK > ALERT); null/unknown counts as no action. */
export function harsher(a, b) {
  const ra = Object.hasOwn(ACTION_RANK, a) ? ACTION_RANK[a] : -1;
  const rb = Object.hasOwn(ACTION_RANK, b) ? ACTION_RANK[b] : -1;
  if (ra < 0 && rb < 0) return null;
  return rb > ra ? b : a;
}

/** The moderation source of the cases: its own once moderationService lists it, else the closest ('automod'). */
export function moderationSource() {
  return MOD_SOURCES.includes('antiraid') ? 'antiraid' : 'automod';
}

/**
 * Splits an @everyone permission bitfield into the send bits a chat lock takes and what is left.
 * @param {bigint} bits
 * @returns {{ taken: bigint, rest: bigint }}
 */
export function splitSend(bits) {
  const b = BigInt(bits);
  return { taken: b & SEND_MASK, rest: b & ~SEND_MASK };
}

/** The saved bits as a bigint; null for no value, 0n for one that is not a number. */
export function parsePrior(text) {
  if (text === null || text === undefined) return null;
  try {
    return BigInt(String(text)) & SEND_MASK; // never more than the send bits, whatever the column holds
  } catch {
    return 0n;
  }
}

/** What @everyone gets when a lock ends: its current permissions plus exactly the saved send bits. */
export function restoredBits(current, priorText) {
  return BigInt(current) | (parsePrior(priorText) ?? 0n);
}

/**
 * The send bits the bot can give back to @everyone: the ones it holds through a role other than @everyone (all of them
 * with Administrator). A bot may only grant permissions it holds itself, and while @everyone lacks a bit the bot has it
 * only through its own roles. Nothing when the bot's roles cannot be read.
 * @returns {bigint}
 */
export function botSendBits(guild) {
  try {
    const me = guild?.members?.me;
    if (!me) return 0n;
    if (me.permissions?.has?.(PermissionFlagsBits.Administrator)) return SEND_MASK;
    let bits = 0n;
    for (const [id, role] of me.roles?.cache ?? []) {
      if ((role?.id ?? id) !== guild.id) bits |= BigInt(role?.permissions?.bitfield ?? 0n);
    }
    return bits & SEND_MASK;
  } catch {
    return 0n;
  }
}

/** The send bits @everyone has that the bot could not give back (a chat lock leaves these alone). */
function blockedSendBits(guild) {
  const bits = guild?.roles?.everyone?.permissions?.bitfield;
  if (bits === undefined || bits === null) return 0n;
  return splitSend(bits).taken & ~botSendBits(guild);
}

/** Why a chat lock leaves bits with @everyone (the same words in the alert and in /antiraid status). */
function blockedChatText(blocked) {
  const many = (blocked & (blocked - 1n)) !== 0n;
  return `Locking chat: none of my roles has **${bitNames(blocked)}**, so I cannot give ${many ? 'them' : 'it'} back and `
    + `@everyone keeps ${many ? 'them' : 'it'}. Give one of my roles ${many ? 'those permissions' : 'that permission'} to lock all of chat.`;
}

/** Display names of the permissions in a bitfield ("Send Messages, Create Public Threads"). */
export function bitNames(bits) {
  const names = permissionNames(new PermissionsBitField(BigInt(bits)).toArray());
  return names.join(', ') || 'none';
}

/**
 * Records one join in the guild's sliding window. When `threshold` joins sit inside `seconds` the window is cleared and
 * `tripped` is true, so one burst starts one lockdown.
 * @param {string} guildId
 * @param {number} at join time (ms)
 * @param {number} now ms
 * @param {number} threshold
 * @param {number} seconds
 * @returns {{ count: number, tripped: boolean }}
 */
export function recordJoin(guildId, at, now, threshold, seconds) {
  const since = now - seconds * 1000;
  const list = (windows.get(guildId) ?? []).filter((t) => t > since);
  const joined = Math.min(at, now);
  if (joined > since) list.push(joined);
  if (list.length > RAID.THRESHOLD.max) list.splice(0, list.length - RAID.THRESHOLD.max);
  if (list.length >= threshold) {
    windows.delete(guildId);
    return { count: list.length, tripped: true };
  }
  if (list.length) windows.set(guildId, list);
  else windows.delete(guildId);
  if (windows.size > 2000 && now - lastSweep > 60_000) sweepWindows(now);
  return { count: list.length, tripped: false };
}

let lastSweep = 0;

function sweepWindows(now) {
  lastSweep = now;
  const since = now - RAID.SECONDS.max * 1000;
  for (const [id, list] of windows) if (!list.length || Math.max(...list) <= since) windows.delete(id);
}

/** The detection settings, clamped to their ranges. */
export function detectionOf(settings) {
  return {
    threshold: clampInt(settings?.antiRaidJoinThreshold, RAID.THRESHOLD, 10),
    seconds: clampInt(settings?.antiRaidJoinSeconds, RAID.SECONDS, 10),
    minutes: clampInt(settings?.antiRaidLockdownMinutes, RAID.MINUTES, RAID.DEFAULT_MINUTES),
  };
}

/**
 * Whether the account is younger than antiRaidMinAccountAgeDays (0 = off).
 * @returns {{ ageDays: number, minDays: number }|null} null when the check is off or the account is old enough
 */
export function youngAccount(user, settings, now) {
  const minDays = clampInt(settings?.antiRaidMinAccountAgeDays, RAID.AGE_DAYS, 0);
  const created = Number(user?.createdTimestamp);
  if (minDays <= 0 || !Number.isFinite(created)) return null;
  const age = now - created;
  if (age >= minDays * DAY_MS) return null;
  return { ageDays: Math.max(0, Math.floor(age / DAY_MS)), minDays };
}

/** True while the row describes a lockdown that has not run out. */
export function isActive(row, now = Date.now()) {
  return Boolean(row?.lockedAt) && Boolean(row?.lockedUntil) && new Date(row.lockedUntil).getTime() > now;
}

/** What the bot lacks for what the settings ask of it. */
export function permissionProblems(guild, settings) {
  const out = [];
  const need = (perm, why) => {
    if (missingBotPermissions(guild, perm).length) out.push(`${why}: I need **${permissionNames(perm)[0]}**.`);
  };
  if (settings?.antiRaidPauseInvites) need('ManageGuild', 'Pausing invites');
  if (settings?.antiRaidLockChat) {
    need('ManageRoles', 'Locking chat');
    const blocked = blockedSendBits(guild);
    if (blocked !== 0n) out.push(blockedChatText(blocked));
  }
  const actions = new Set([settings?.antiRaidAction]);
  if (clampInt(settings?.antiRaidMinAccountAgeDays, RAID.AGE_DAYS, 0) > 0) actions.add(settings?.antiRaidYoungAction);
  if (actions.has('KICK')) need('KickMembers', 'Kicking members');
  if (actions.has('BAN')) need('BanMembers', 'Banning members');
  return out;
}

/** The settings as lines for /antiraid status and config. */
export function configLines(settings) {
  const d = detectionOf(settings);
  const age = clampInt(settings?.antiRaidMinAccountAgeDays, RAID.AGE_DAYS, 0);
  return [
    `**Detection:** ${d.threshold} joins within ${d.seconds} seconds`,
    `**Members who join during a raid:** ${ACTION_LABEL[settings?.antiRaidAction] ?? ACTION_LABEL.ALERT}`,
    `**Lockdown:** ${formatDurationLong(d.minutes * MINUTE_MS)}, invites ${settings?.antiRaidPauseInvites ? 'paused' : 'left open'}, `
      + `chat ${settings?.antiRaidLockChat ? 'locked' : 'left open'}`,
    `**New accounts:** ${age > 0 ? `younger than ${age} day${age === 1 ? '' : 's'}: ${(ACTION_LABEL[settings?.antiRaidYoungAction] ?? '').toLowerCase()}` : 'not checked'}`,
    `**Alerts:** ${settings?.antiRaidAlertChannelId ? `<#${settings.antiRaidAlertChannelId}>` : (settings?.modLogChannelId ? `<#${settings.modLogChannelId}> (moderation log)` : 'no channel set')}`,
  ];
}

const auditReason = (text) => clampText(`Anti-Raid: ${text}`, RAID.AUDIT_REASON_MAX);

// ── State ─────────────────────────────────────────────────────────────────────

/** The guild's AntiRaidState row, or null. Throws on a database failure. */
export function loadState(guildId) {
  return prisma.antiRaidState.findUnique({ where: { guildId } });
}

/** Counters of what the lockdown did, for its closing note (not kept across restarts). */
export function tallyOf(guildId) {
  let t = tallies.get(guildId);
  if (!t) {
    t = { kicked: 0, banned: 0, failed: 0, flagged: 0 };
    tallies.set(guildId, t);
  }
  return t;
}

/** Sets the memo to "a lockdown is on until `until`" (also used before the row exists, so joiners are caught at once). */
export function markActive(guildId, until) {
  memo.set(guildId, { active: true, until, at: Date.now() });
}

function markNone(guildId) {
  memo.set(guildId, { active: false, until: 0, at: Date.now() });
}

/** Memoises "no lockdown" unless one became active meanwhile; false when it did (the memo is left alone). */
function markNoneUnlessActive(guildId, now) {
  const m = memo.get(guildId);
  if (m?.active && m.until > now) return false;
  markNone(guildId);
  return true;
}

/** Sync answer from the memo: true / false, or undefined when the table has to be asked. */
export function peekLocked(guildId, now = Date.now()) {
  const m = memo.get(guildId);
  if (!m) return undefined;
  if (m.active) return m.until > now ? true : undefined;
  return now - m.at < RAID.NONE_MEMO_MS ? false : undefined;
}

/**
 * Whether a lockdown is on. Memo first; one table read otherwise. A row that ran out without being ended (its job was
 * lost) is ended in the background. Never throws: a failed read counts as "no lockdown".
 * @param {object} guild
 * @param {number} [now]
 * @param {object} [deps]
 * @returns {Promise<boolean>}
 */
export async function isLocked(guild, now = Date.now(), deps = {}) {
  const fast = peekLocked(guild.id, now);
  if (fast !== undefined) return fast;
  try {
    const row = await loadState(guild.id);
    if (isActive(row, now)) {
      markActive(guild.id, new Date(row.lockedUntil).getTime());
      return true;
    }
    // A raid may have tripped while the read was in flight: its "active" memo must not be overwritten by this stale answer.
    if (!markNoneUnlessActive(guild.id, now)) return true;
    if (row?.lockedAt) {
      endLockdown({ guild, onlyIfDue: true }, deps)
        .then((res) => (res.wasActive ? announceEnd(guild, null, res, { how: 'expired' }) : null))
        .catch((err) => logger.warn(`[antiraid] ${guild.id}: could not end an expired lockdown: ${err?.message}`));
    }
    return false;
  } catch (err) {
    if (noAlertThrottle(`${guild.id}:state`)) logger.warn(`[antiraid] ${guild.id}: could not read the lockdown state: ${err?.message}`);
    return false;
  }
}

// ── Starting a lockdown ───────────────────────────────────────────────────────

/** Error text of a failed Discord call. */
function why(err) {
  if (err?.code === 50013 || err?.code === 50001) return 'Discord refused (missing permission or my role is too low)';
  return clampText(err?.message ?? 'unknown error', 150);
}

async function takeChat({ guild, settings, existing, effects }) {
  // Returns { prior: string|null, strip: bigint|null, taken: bigint, blocked: bigint }. prior is what gets saved before the
  // role is edited; blocked is the send bits the bot could not give back, which are left alone.
  const pending = parsePrior(existing?.priorEveryoneSend); // a restore left over from an earlier lockdown is kept
  const keep = { prior: existing?.priorEveryoneSend ?? null, strip: null, taken: 0n, blocked: 0n };
  if (!settings?.antiRaidLockChat) return keep;
  if (missingBotPermissions(guild, 'ManageRoles').length) {
    effects.chat = 'failed';
    effects.errors.push('Chat not locked: I need **Manage Roles**.');
    return keep;
  }
  const everyone = guild.roles?.everyone;
  if (!everyone) {
    effects.chat = 'failed';
    effects.errors.push('Chat not locked: the @everyone role was not found.');
    return keep;
  }
  const bits = BigInt(everyone.permissions.bitfield);
  const taken = splitSend(bits).taken & botSendBits(guild); // only what can be given back afterwards
  const blocked = splitSend(bits).taken & ~taken;
  const total = (pending ?? 0n) | taken;
  return { prior: total === 0n && pending === null ? null : total.toString(), strip: bits & ~taken, taken, blocked };
}

/** Pauses invites until `until`. Returns that end when the bot set the pause, else null (failed, or a longer pause was on). */
async function pauseInvites({ guild, until, effects }) {
  if (missingBotPermissions(guild, 'ManageGuild').length) {
    effects.invites = 'failed';
    effects.errors.push('Invites not paused: I need **Manage Server**.');
    return null;
  }
  const current = guild.incidentsData?.invitesDisabledUntil;
  if (current && new Date(current).getTime() >= until.getTime()) {
    effects.invites = 'already';
    return null;
  }
  const payload = { invitesDisabledUntil: until };
  const dms = guild.incidentsData?.dmsDisabledUntil; // keep a DM pause somebody else set
  if (dms && new Date(dms).getTime() > Date.now()) payload.dmsDisabledUntil = dms;
  try {
    await guild.setIncidentActions(payload);
    effects.invites = 'paused';
    return until;
  } catch (err) {
    effects.invites = 'failed';
    effects.errors.push(`Invites not paused: ${why(err)}.`);
    return null;
  }
}

/**
 * Starts a lockdown unless one is on. The row is saved and the unlock job scheduled first; then invites are paused and
 * chat locked as the settings ask, each failing alone (the lockdown stands and `effects.errors` says what is missing).
 * Serialised per guild, so a burst of joins starts exactly one. `trigger` ({ joins, seconds }) marks an automatic start.
 * @param {{ guild: object, settings?: object|null, startedById?: string|null, reason?: string|null, minutes?: number|null,
 *   trigger?: { joins: number, seconds: number }|null }} input
 * @param {object} [deps]
 * @returns {Promise<{ ok: true, started: boolean, state: object, minutes?: number, effects?: object }
 *   | { ok: false, error: string }>} never throws
 */
export function startLockdown(input, deps = {}) {
  return mutex.run(input.guild.id, async () => {
    try {
      return await doStart(input, { ...DEPS, ...deps });
    } catch (err) {
      logger.error(`[antiraid] ${input.guild.id}: could not start a lockdown`, err);
      memo.delete(input.guild.id);
      return { ok: false, error: 'The lockdown could not be saved. Please try again in a moment.' };
    }
  });
}

async function doStart({ guild, settings = null, startedById = null, reason = null, minutes = null }, d) {
  const guildId = guild.id;
  const now = Date.now();
  const existing = await loadState(guildId);
  if (isActive(existing, now)) {
    markActive(guildId, new Date(existing.lockedUntil).getTime());
    return { ok: true, started: false, state: existing };
  }
  const s = settings ?? await getSettingsSafe(guildId);
  const mins = clampInt(minutes ?? s?.antiRaidLockdownMinutes, RAID.MINUTES, RAID.DEFAULT_MINUTES);
  const lockedAt = new Date(now);
  const lockedUntil = new Date(now + mins * MINUTE_MS);
  const effects = { chat: 'skipped', invites: 'skipped', errors: [] };
  const text = reason ? clampText(String(reason).trim(), RAID.REASON_MAX) || null : null;

  const chat = await takeChat({ guild, settings: s, existing, effects });
  const data = { lockedAt, lockedUntil, lockedById: startedById, reason: text, priorEveryoneSend: chat.prior, invitesPausedUntil: null };
  await d.ensureGuildSettings(guildId);
  // Saved before anything is touched: a crash after this leaves a lockdown the job will end, never lost bits.
  const state = await prisma.antiRaidState.upsert({ where: { guildId }, create: { guildId, ...data }, update: data });
  markActive(guildId, lockedUntil.getTime());

  try {
    await d.scheduleJob({
      guildId, type: RAID.UNLOCK_JOB, runAt: lockedUntil, payload: {}, dedupeKey: `${RAID.UNLOCK_JOB}:${guildId}`, onConflict: 'replace',
    });
  } catch (err) {
    // No job would ever end it: undo the row.
    await prisma.antiRaidState.update({
      where: { guildId },
      data: {
        lockedAt: null, lockedUntil: null, lockedById: null, reason: null, priorEveryoneSend: existing?.priorEveryoneSend ?? null,
        invitesPausedUntil: null,
      },
    }).catch((e) => logger.error(`[antiraid] ${guildId}: could not undo the lockdown row`, e));
    memo.delete(guildId);
    throw err;
  }

  windows.delete(guildId);

  if (chat.strip !== null) {
    if (chat.taken === 0n) {
      effects.chat = chat.blocked === 0n ? 'nothing' : 'failed';
      if (chat.blocked !== 0n) effects.errors.push(blockedChatText(chat.blocked));
    } else {
      try {
        await guild.roles.everyone.setPermissions(chat.strip, auditReason('raid lockdown'));
        effects.chat = chat.blocked === 0n ? 'locked' : 'partial';
        if (chat.blocked !== 0n) effects.errors.push(blockedChatText(chat.blocked));
      } catch (err) {
        effects.chat = 'failed';
        effects.errors.push(`Chat not locked: ${why(err)}.`);
        // Nothing was taken, so there is nothing to give back (keep an earlier pending restore).
        await prisma.antiRaidState.update({ where: { guildId }, data: { priorEveryoneSend: existing?.priorEveryoneSend ?? null } })
          .catch((e) => logger.error(`[antiraid] ${guildId}: could not reset the saved permissions`, e));
        state.priorEveryoneSend = existing?.priorEveryoneSend ?? null;
      }
    }
  }
  if (s?.antiRaidPauseInvites) {
    // The end the bot really set (capped under lockedUntil for a 24 hour lockdown) is saved: ending compares against it.
    const pausedUntil = await pauseInvites({ guild, until: new Date(Math.min(lockedUntil.getTime(), Date.now() + RAID.INVITE_CAP_MS)), effects });
    if (pausedUntil) {
      state.invitesPausedUntil = pausedUntil;
      await prisma.antiRaidState.update({ where: { guildId }, data: { invitesPausedUntil: pausedUntil } })
        .catch((e) => logger.error(`[antiraid] ${guildId}: could not save the invite pause`, e));
    }
  }
  logger.info(`[antiraid] ${guildId}: lockdown until ${lockedUntil.toISOString()} (${startedById ? `by ${startedById}` : 'automatic'}); chat ${effects.chat}, invites ${effects.invites}`);
  return { ok: true, started: true, state, minutes: mins, effects };
}

// ── Ending a lockdown ─────────────────────────────────────────────────────────

async function restoreEveryone(guild, priorText) {
  const everyone = guild.roles?.everyone;
  if (!everyone) return { ok: true, changed: false };
  const current = BigInt(everyone.permissions.bitfield);
  const owed = restoredBits(current, priorText) & ~current; // saved bits @everyone still lacks
  if (owed === 0n) return { ok: true, changed: false };
  const names = bitNames(owed);
  if (missingBotPermissions(guild, 'ManageRoles').length) {
    return { ok: false, permanent: true, error: `I need **Manage Roles** to give @everyone back: ${names}. Run \`/antiraid lockdown end\` once I have it.` };
  }
  // A bot may only grant what it holds through its own roles; a bit it cannot grant (saved by an older lockdown, or a role
  // that lost it meanwhile) must not stop the others from coming back. It stays saved, and is named, for the next end.
  const stuck = owed & ~botSendBits(guild);
  const give = owed & ~stuck;
  let changed = false;
  if (give !== 0n) {
    try {
      await everyone.setPermissions(current | give, auditReason('lockdown ended'));
      changed = true;
    } catch (err) {
      return {
        ok: false, permanent: err?.code === 50013 || err?.code === 50001,
        error: `Could not give @everyone back ${names}: ${why(err)}. Run \`/antiraid lockdown end\` to try again.`,
      };
    }
  }
  if (stuck !== 0n) {
    const many = (stuck & (stuck - 1n)) !== 0n;
    return {
      ok: false, permanent: true, changed, keep: stuck.toString(),
      error: `@everyone still lacks ${bitNames(stuck)}: none of my roles has ${many ? 'them' : 'it'}, so I cannot give ${many ? 'them' : 'it'} back. `
        + `Give one of my roles ${many ? 'those permissions' : 'that permission'} (or turn ${many ? 'them' : 'it'} on for @everyone yourself), then run \`/antiraid lockdown end\`.`,
    };
  }
  return { ok: true, changed };
}

/** Resumes the invites the bot paused: the guild's pause must end where the one saved at start ends (null = the bot paused none). */
async function resumeInvites(guild, row) {
  const current = guild.incidentsData?.invitesDisabledUntil ? new Date(guild.incidentsData.invitesDisabledUntil).getTime() : null;
  const ours = row.invitesPausedUntil ? new Date(row.invitesPausedUntil).getTime() : null;
  if (current !== null && current <= Date.now()) return { state: 'none' };
  if (current !== null && (ours === null || Math.abs(current - ours) > RAID.INVITE_MATCH_MS)) return { state: 'foreign' }; // not ours
  if (current === null && ours === null) return { state: 'none' };
  if (missingBotPermissions(guild, 'ManageGuild').length) {
    return current === null ? { state: 'none' } : { state: 'failed', error: 'Invites stay paused: I need **Manage Server** to resume them.' };
  }
  const payload = { invitesDisabledUntil: null };
  const dms = guild.incidentsData?.dmsDisabledUntil;
  if (dms && new Date(dms).getTime() > Date.now()) payload.dmsDisabledUntil = dms;
  try {
    await guild.setIncidentActions(payload);
    return { state: current === null ? 'none' : 'resumed' };
  } catch (err) {
    return current === null ? { state: 'none' } : { state: 'failed', error: `Invites stay paused: ${why(err)}.` };
  }
}

/**
 * Ends the lockdown: gives @everyone its send permissions back, resumes invites, clears the row and the unlock job.
 * Idempotent: with no lockdown it does nothing. A chat restore that fails keeps the saved bits (the row is cleared
 * otherwise) so another call retries it; bits the bot cannot grant are named and stay saved while the others come back.
 * Invites are resumed only when the guild's pause ends where AntiRaidState.invitesPausedUntil says the bot's did.
 * `onlyIfDue` (the unlock job) leaves a lockdown that is not due yet alone. Throws only on a database failure.
 * @param {{ guild: object, onlyIfDue?: boolean }} input
 * @param {object} [deps]
 * @returns {Promise<{ wasActive: boolean, restored: boolean, pendingRestore: boolean, permanent: boolean, row: object|null,
 *   effects: { chat: string, invites: string, errors: string[] }, tally: object|null }>}
 */
export function endLockdown({ guild, onlyIfDue = false }, deps = {}) {
  const d = { ...DEPS, ...deps };
  const guildId = guild.id;
  return mutex.run(guildId, async () => {
    const now = Date.now();
    const none = (extra = {}) => ({
      wasActive: false, restored: false, pendingRestore: false, permanent: false, row: null,
      effects: { chat: 'skipped', invites: 'skipped', errors: [] }, tally: null, ...extra,
    });
    const row = await loadState(guildId);
    const active = Boolean(row?.lockedAt);
    const pending = row?.priorEveryoneSend !== null && row?.priorEveryoneSend !== undefined;
    if (!row || (!active && !pending)) {
      markNone(guildId);
      return none();
    }
    if (onlyIfDue && active && row.lockedUntil && new Date(row.lockedUntil).getTime() > now + RAID.DUE_SLACK_MS) return none({ row });

    windows.delete(guildId); // the burst that caused this must not start the next one
    const effects = { chat: 'skipped', invites: 'skipped', errors: [] };
    let permanent = false;
    let keepPrior = false;
    let keepValue = null; // what stays saved when a restore did not complete

    if (pending) {
      const r = await restoreEveryone(guild, row.priorEveryoneSend);
      if (r.ok) {
        effects.chat = r.changed ? 'restored' : 'unchanged';
      } else {
        effects.chat = r.changed ? 'partialRestore' : 'failed';
        effects.errors.push(r.error);
        keepPrior = true;
        keepValue = r.keep ?? row.priorEveryoneSend; // only the bits still owed when some did come back
        permanent = r.permanent === true;
      }
    }
    if (active) {
      const r = await resumeInvites(guild, row);
      effects.invites = r.state;
      if (r.error) effects.errors.push(r.error);
    }

    await prisma.antiRaidState.update({
      where: { guildId },
      data: { lockedAt: null, lockedUntil: null, lockedById: null, reason: null, priorEveryoneSend: keepPrior ? keepValue : null, invitesPausedUntil: null },
    });
    markNone(guildId);
    // A failed restore keeps the job: the scheduler retries it (a job that throws is retried).
    if (!keepPrior) await d.cancelJobs({ dedupeKey: `${RAID.UNLOCK_JOB}:${guildId}` });
    const tally = tallies.get(guildId) ?? null;
    tallies.delete(guildId);
    logger.info(`[antiraid] ${guildId}: lockdown ended; chat ${effects.chat}, invites ${effects.invites}`);
    return { wasActive: active, restored: effects.chat === 'restored', pendingRestore: keepPrior, permanent, row, effects, tally };
  });
}

// ── Alerts ────────────────────────────────────────────────────────────────────

const INVITE_TEXT = Object.freeze({
  paused: '⏸️ Paused', already: '⏸️ Already paused for longer', failed: '⚠️ Not paused', skipped: 'Left open',
  none: '—', resumed: '▶️ Resumed', foreign: 'Left as it was (not paused by me)',
});
const CHAT_TEXT = Object.freeze({
  locked: '🔇 @everyone cannot send messages', nothing: '🔇 @everyone could not send already', failed: '⚠️ Not locked',
  partial: '🔇 Partly locked (see below)', partialRestore: '⚠️ Only partly allowed again (see below)',
  skipped: 'Left open', restored: '🔊 Sending allowed again', unchanged: '🔊 Already allowed', none: '—',
});

/** The "End lockdown" button row of an alert. */
export function endButtonRow() {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('antiraid', 'end')).setLabel('End lockdown').setEmoji('🔓').setStyle(ButtonStyle.Danger),
  );
}

/**
 * The alert for a started lockdown.
 * @param {{ state: object, settings?: object|null, effects?: object, trigger?: { joins: number, seconds: number }|null, problems?: string[] }} input
 */
export function buildStartEmbed({ state, settings = null, effects = null, trigger = null, problems = [] }) {
  const until = new Date(state.lockedUntil);
  const lines = [];
  if (trigger) lines.push(`**${trigger.joins}** members joined within **${trigger.seconds}** seconds.`);
  else lines.push(`Started by ${state.lockedById ? `<@${state.lockedById}>` : 'a moderator'}${state.reason ? `: ${clampText(state.reason, RAID.REASON_MAX)}` : '.'}`);
  const embed = neutralEmbed(trigger ? '🚨 Raid detected: lockdown started' : '🔒 Lockdown started', lines.join('\n'))
    .setColor(COLORS.ERROR)
    .addFields(
      { name: 'Ends', value: `${discordTimestamp(until, 'R')} (${discordTimestamp(until, 't')})`, inline: true },
      { name: 'New members', value: ACTION_LABEL[settings?.antiRaidAction] ?? ACTION_LABEL.ALERT, inline: true },
      { name: 'Invites', value: INVITE_TEXT[effects?.invites ?? 'skipped'], inline: true },
      { name: 'Chat', value: CHAT_TEXT[effects?.chat ?? 'skipped'], inline: true },
    )
    .setFooter({ text: 'Moderators can end the lockdown with the button.' });
  const all = [...(effects?.errors ?? []), ...problems];
  if (all.length) embed.addFields({ name: 'Needs attention', value: clampText([...new Set(all)].map((p) => `• ${p}`).join('\n'), 1024) });
  return embed;
}

/** The note posted when a lockdown ends. `how` is 'expired' or 'manual'. */
export function buildEndEmbed({ how = 'manual', endedById = null, result = null }) {
  const lines = [how === 'expired' ? 'The lockdown ran out.' : `Ended by ${endedById ? `<@${endedById}>` : 'a moderator'}.`];
  const t = result?.tally;
  if (t && (t.kicked || t.banned || t.failed || t.flagged)) {
    const parts = [];
    if (t.kicked) parts.push(`${t.kicked} kicked`);
    if (t.banned) parts.push(`${t.banned} banned`);
    if (t.flagged) parts.push(`${t.flagged} reported`);
    if (t.failed) parts.push(`${t.failed} could not be removed`);
    lines.push(`Members who joined meanwhile: ${parts.join(', ')}.`);
  }
  const e = result?.effects;
  const embed = neutralEmbed('🔓 Raid lockdown ended', lines.join('\n')).setColor(result?.pendingRestore ? COLORS.WARNING : COLORS.SUCCESS);
  if (e) {
    embed.addFields(
      { name: 'Invites', value: INVITE_TEXT[e.invites] ?? '—', inline: true },
      { name: 'Chat', value: CHAT_TEXT[e.chat] ?? '—', inline: true },
    );
    if (e.errors.length) embed.addFields({ name: 'Needs attention', value: clampText(e.errors.map((p) => `• ${p}`).join('\n'), 1024) });
  }
  return embed;
}

/** The alert for new accounts. */
export function buildYoungEmbed({ member, ageDays, minDays, action, more = 0 }) {
  const user = member?.user ?? member;
  const done = { KICK: 'Kicked', BAN: 'Banned', ALERT: 'Only reported' }[action] ?? 'Only reported';
  const embed = neutralEmbed('🆕 New account joined', `<@${user?.id}> (${clampText(user?.tag ?? user?.username ?? user?.id, 80)})`)
    .setColor(COLORS.WARNING)
    .addFields(
      { name: 'Account age', value: `${ageDays} day${ageDays === 1 ? '' : 's'} (minimum ${minDays})`, inline: true },
      { name: 'Action', value: done, inline: true },
    );
  if (more > 0) embed.addFields({ name: 'Since the last alert', value: `${more} more new account${more === 1 ? '' : 's'}` });
  return embed;
}

async function alertChannel(guild, settings) {
  for (const id of [settings?.antiRaidAlertChannelId, settings?.modLogChannelId]) {
    if (!id) continue;
    const channel = guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
    if (checkChannelUsable(channel).ok) return channel;
  }
  return null;
}

/**
 * Posts an alert in the alert channel (else the moderation log channel). Never pings; never throws.
 * @returns {Promise<{ channel: object, message: object }|null>} null when nothing could be posted
 */
export async function postAlert(guild, settings, payload) {
  try {
    const s = settings ?? await getSettingsSafe(guild.id);
    const channel = await alertChannel(guild, s);
    if (!channel) {
      if (noAlertThrottle(guild.id)) logger.warn(`[antiraid] ${guild.id}: no usable alert channel; set one with /antiraid config`);
      return null;
    }
    const message = await channel.send({ ...payload, allowedMentions: { parse: [] } });
    return { channel, message };
  } catch (err) {
    if (!ALERT_GONE_CODES.has(err?.code)) logger.warn(`[antiraid] ${guild?.id}: could not post an alert: ${err?.message}`);
    return null;
  }
}

/** Posts the alert of a started lockdown. */
export function announceStart(guild, settings, started, trigger = null) {
  return postAlert(guild, settings, {
    embeds: [buildStartEmbed({ state: started.state, settings, effects: started.effects, trigger, problems: permissionProblems(guild, settings) })],
    components: [endButtonRow()],
  });
}

/** Posts the closing note of a lockdown (skipped in `exceptChannelId`, where a reply already says it). */
export async function announceEnd(guild, settings, result, { how = 'manual', endedById = null, exceptChannelId = null } = {}) {
  const s = settings ?? await getSettingsSafe(guild.id);
  if (exceptChannelId) {
    const channel = await alertChannel(guild, s).catch(() => null);
    if (channel?.id === exceptChannelId) return null;
  }
  return postAlert(guild, s, { embeds: [buildEndEmbed({ how, endedById, result })] });
}

/**
 * Counts one new account for the throttled alert. Posts it when the throttle allows; the accounts seen in between are
 * counted into the next one. Never throws.
 */
export async function reportYoung(guild, settings, { member, ageDays, minDays, action }) {
  try {
    const pending = youngPending.get(guild.id) ?? { count: 0, latest: null };
    pending.count++;
    pending.latest = { member, ageDays, minDays, action };
    youngPending.set(guild.id, pending);
    if (!youngThrottle(guild.id)) return null;
    youngPending.delete(guild.id);
    return await postAlert(guild, settings, { embeds: [buildYoungEmbed({ ...pending.latest, more: pending.count - 1 })] });
  } catch (err) {
    logger.warn(`[antiraid] ${guild?.id}: new-account alert failed: ${err?.message}`);
    return null;
  }
}

// ── Acting on a member ────────────────────────────────────────────────────────

/**
 * Kicks or bans a joiner through moderation (a case in the mod log, no DM, no escalation). Runs a few at a time.
 * @param {{ guild: object, member: object, action: 'KICK'|'BAN', reason: string, detail?: string }} input
 * @param {object} [deps]
 * @returns {Promise<{ ok: boolean, skipped?: boolean, code?: string, error?: string }>} never throws
 */
export async function removeMember({ guild, member, action, reason, detail = null }, deps = {}) {
  const run = deps.executeModAction ?? DEPS.executeModAction;
  try {
    const res = await actionSlots.run(() => run({
      guild,
      target: member,
      moderator: guild.client.user,
      action,
      reason: clampText(reason, RAID.REASON_MAX),
      source: moderationSource(),
      silent: true,
      skipEscalation: true,
      ...(action === 'BAN' ? { deleteMessageSeconds: RAID.BAN_DELETE_SECONDS } : {}),
      ...(detail ? { logFields: [{ name: 'Anti-Raid', value: clampText(detail, 300) }] } : {}),
    }));
    // `applied`: the member was removed but the case could not be saved. Removing them again would fail, so it counts.
    if (res?.ok || res?.applied === true) return { ok: true };
    // Staff, the owner, someone already gone or already banned: nothing to do and nothing wrong.
    if (['IMMUNE', 'NOT_IN_GUILD', 'ALREADY'].includes(res?.code)) return { ok: false, skipped: true, code: res.code };
    if (noAlertThrottle(`${guild.id}:remove`)) logger.warn(`[antiraid] ${guild.id}: could not ${action.toLowerCase()} ${member.id}: ${res?.error ?? res?.code}`);
    return { ok: false, code: res?.code, error: res?.error };
  } catch (err) {
    logger.warn(`[antiraid] ${guild?.id}: ${action} of ${member?.id} failed: ${err?.message}`);
    return { ok: false, code: 'ERROR', error: err?.message };
  }
}
