// Server Boosts: a thank-you when a member boosts the server, and an optional role of their own for boosters (/boost role).
// This file holds the rules (who may do what, what a booster role may look like), the BoosterRole table access, the Discord
// role calls and the `boosts.sweep` job; the hooks are declared in src/plugins/boosts.js and the command is
// src/commands/community/boost.js.
//
// How the pieces fit:
//  - Boost start / end come from guildMemberUpdate (old vs new `premiumSince`); a partial old member is never diffed.
//  - A booster role is created (permissions 0, not hoisted, not mentionable), moved just below the anchor role (if set), else
//    just above the server's Nitro Booster role, else left at the bottom, and given to the member. Never at or above the
//    bot's highest role, and low in the hierarchy by default, so moderators can still act on boosters. Any step failing
//    after the create deletes the role again.
//  - boosts.sweep is the repair path: it deletes roles whose grace period is over or whose owner left, and fixes rows whose
//    member stopped or resumed boosting while the bot was not looking. It keeps cleaning while the module is off.
import { PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { isModuleEnabled } from '../config/modules.js';
import { ensureGuildSettings, getSettingsSafe, peekSettings, updateSettings } from './settingsService.js';
import { getJob, scheduleJob } from './scheduler.js';
import { createKeyedMutex, createThrottle, withTimeout } from '../utils/async.js';
import { errorEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable } from '../utils/interactionHelpers.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';
import { safeFetch } from '../utils/safeFetch.js';
import { clampText } from '../utils/text.js';
import logger from '../utils/logger.js';

export const BOOST = Object.freeze({
  JOB_TYPE: 'boosts.sweep',
  DAY_MS: 86_400_000,
  GRACE_MAX_DAYS: 30,
  // The sweep runs about hourly; a run that had more to do than it may (see below) comes back sooner.
  SWEEP_EVERY_MS: 3_600_000,
  SWEEP_MORE_MS: 60_000,
  SWEEP_RETRY_MS: 5 * 60_000,
  STARTUP_DELAY_MS: 60_000,
  STARTUP_JITTER_MS: 120_000,
  // Per run: members looked up over the API (the cache is free) and roles deleted. Discord allows 250 roles per server,
  // so a guild never has more rows than that; the rest of a long list is done by the next run.
  LOOKUPS_PER_RUN: 50,
  DELETES_PER_RUN: 25,
  LOOKUP_TIMEOUT_MS: 8_000,
  // No lookup or row starts after this long, so a slow Discord never pushes a run past the scheduler's 60 s job timeout
  // (5 timeouts in a row would leave the job FAILED); what is left is done by the next run, a minute later.
  SWEEP_BUDGET_MS: 35_000,
  ROLES_PER_GUILD: 250,
  // One create or edit per member per minute (in memory).
  EDIT_COOLDOWN_MS: 60_000,
  COOLDOWN_MAX: 5_000,
  ICON_TIMEOUT_MS: 8_000,
  EMOJI_MAX: 32,
  MESSAGE_MAX: 2000,
  REASON_MAX: 400,
});

/** The thank-you used while `boostMessage` is null. Same text as DEFAULT_TEXTS.boostMessage in src/dashboard/fields.js. */
export const DEFAULT_BOOST_MESSAGE = '💎 {user} just boosted **{server}**! Thank you! We now have {boosts} boosts.';

const NO_MENTIONS = Object.freeze({ parse: [] });
const LABEL = 'Server Boosts';

// One member's create / edit / delete / end / leave run one at a time.
const lock = createKeyedMutex();
export const runLocked = (guildId, userId, fn) => lock.run(`${guildId}:${userId}`, fn);

const warnBlocked = createThrottle(3_600_000);
const reason = (text) => clampText(`${LABEL}: ${text}`, BOOST.REASON_MAX, '');

// ── Pure rules ────────────────────────────────────────────────────────────────

/** True when the member is boosting the server (reads the timestamp, so a cheap fake works too). */
export function isBoosting(member) {
  const ts = member?.premiumSinceTimestamp;
  if (ts !== undefined && ts !== null) return Number(ts) > 0;
  return Boolean(member?.premiumSince);
}

/**
 * What changed between two states of a member: 'start' (not boosting, now boosting), 'end' or null. The caller makes sure
 * the old member was cached (ctx.oldKnown); a partial one has no usable premiumSince.
 */
export function boostTransition(oldMember, newMember) {
  const before = isBoosting(oldMember);
  const after = isBoosting(newMember);
  if (!before && after) return 'start';
  if (before && !after) return 'end';
  return null;
}

/** Milliseconds a member keeps the role after the boost ended (`boostRoleGraceDays`, 0..30). */
export function graceMs(settings) {
  const d = Math.trunc(Number(settings?.boostRoleGraceDays));
  if (!Number.isFinite(d) || d <= 0) return 0;
  return Math.min(BOOST.GRACE_MAX_DAYS, d) * BOOST.DAY_MS;
}

/** The server's boost tier, 0..3. */
export function tierNumber(guild) {
  const t = Math.trunc(Number(guild?.premiumTier));
  return Number.isFinite(t) ? Math.min(3, Math.max(0, t)) : 0;
}

/** The configured text, or the default when it is null or blank. */
export function messageTemplate(settings) {
  const own = typeof settings?.boostMessage === 'string' ? settings.boostMessage : '';
  return own.trim() ? own : DEFAULT_BOOST_MESSAGE;
}

/** The thank-you for a member, placeholders rendered ({boosts} = boost count, {tier} = 0..3), at most 2000 characters. */
export function renderThankYou(settings, member, guild, { maxLength = BOOST.MESSAGE_MAX } = {}) {
  const text = renderPlaceholders(messageTemplate(settings), {
    member, guild, vars: { boosts: Number(guild?.premiumSubscriptionCount ?? 0), tier: tierNumber(guild) },
  }, { maxLength });
  return text.trim() || renderPlaceholders(DEFAULT_BOOST_MESSAGE, {
    member, guild, vars: { boosts: Number(guild?.premiumSubscriptionCount ?? 0), tier: tierNumber(guild) },
  }, { maxLength });
}

/** `#rrggbb` (the # is optional) to an integer; null/blank is "no colour given". */
export function parseColor(raw) {
  if (raw === null || raw === undefined || String(raw).trim() === '') return { ok: true, value: null };
  const m = /^#?([0-9a-f]{6})$/i.exec(String(raw).trim());
  return m ? { ok: true, value: Number.parseInt(m[1], 16) } : { ok: false };
}

// One standard emoji: a pictograph with skin tone / variation selector / tag characters, joined by ZWJ; a flag; a keycap.
// Two emojis side by side, text and custom emojis `<:a:1>` are refused.
const PICTO = '\\p{Extended_Pictographic}(?:\\uFE0F|\\p{Emoji_Modifier}|[\\u{E0020}-\\u{E007F}])*';
const EMOJI_RE = new RegExp('^(?:' + PICTO + '(?:\\u200D' + PICTO + ')*|\\p{Regional_Indicator}{2}|[0-9#*]\\uFE0F?\\u20E3)$', 'u');

export function parseEmoji(raw) {
  const s = String(raw ?? '').trim();
  if (!s || s.length > BOOST.EMOJI_MAX || !EMOJI_RE.test(s)) return { ok: false };
  return { ok: true, emoji: s };
}

const INVISIBLE_RE = new RegExp('[\\u0000-\\u001f\\u007f-\\u009f\\u00ad\\u200b-\\u200f\\u2028-\\u202e\\u2060-\\u206f\\ufeff]', 'g');
const AT_RE = new RegExp('[@\\uff20\\ufe6b]');
const LINK_RE = new RegExp('(?:https?:\\/\\/|www\\.|\\bdiscord(?:app)?\\.(?:gg|com)\\b'
  + '|\\b[a-z0-9-]+\\.(?:com|net|org|gg|io|me|tv|xyz|co|app|dev|ly|to|gl|link|info|biz|us|uk|de|ru)\\b)', 'i');
const fold = (s) => s.normalize('NFKC').toLowerCase();
const skeleton = (s) => fold(s).replace(/[^\p{L}\p{N}]+/gu, '');

/**
 * Checks a booster role name: trimmed (invisible characters dropped, spaces collapsed), 1..BOOSTER_ROLE_NAME characters,
 * no @, no links or invites, and not the name of an existing role (case-insensitive, also when only punctuation, spaces or
 * look-alike width forms differ), so nobody can make a fake "Moderator". `exceptRoleId` is the caller's own role.
 * @returns {{ ok: true, name: string } | { ok: false, reason: 'empty'|'too-long'|'at'|'link'|'taken' }}
 */
export function validateRoleName(raw, guild, { exceptRoleId = null } = {}) {
  const name = String(raw ?? '').replace(INVISIBLE_RE, '').replace(/\s+/g, ' ').trim();
  if (!name) return { ok: false, reason: 'empty' };
  if (name.length > LIMITS.BOOSTER_ROLE_NAME) return { ok: false, reason: 'too-long' };
  if (AT_RE.test(name)) return { ok: false, reason: 'at' };
  if (LINK_RE.test(name)) return { ok: false, reason: 'link' };
  const folded = fold(name);
  const skel = skeleton(name);
  for (const role of guild?.roles?.cache?.values?.() ?? []) {
    if (role.id === exceptRoleId) continue;
    const other = String(role.name ?? '');
    if (fold(other) === folded || (skel && skeleton(other) === skel)) return { ok: false, reason: 'taken' };
  }
  return { ok: true, name };
}

const NAME_REASONS = Object.freeze({
  empty: 'Give the role a name.',
  'too-long': `Role names are at most ${LIMITS.BOOSTER_ROLE_NAME} characters.`,
  at: 'A role name cannot contain `@`.',
  link: 'A role name cannot contain a link or an invite.',
  taken: 'A role with that name already exists. Pick a different name.',
});

/**
 * What the sweep does with one row, given what is known about the member.
 * @param {{ boostEndedAt: Date|null }} row
 * @param {object|null|undefined} member the GuildMember; `null` = left the server, `undefined` = could not be looked up
 * @returns {{ type: 'none'|'skip'|'delete'|'ended'|'resumed', why?: string }}
 */
export function planSweep(row, member, settings, now = Date.now()) {
  if (member === undefined) return { type: 'skip' };
  if (member === null) return { type: 'delete', why: 'the member left the server' };
  const ended = row?.boostEndedAt ? new Date(row.boostEndedAt).getTime() : null;
  if (isBoosting(member)) return ended === null ? { type: 'none' } : { type: 'resumed' };
  const grace = graceMs(settings);
  // Stopped boosting while the bot was not looking: with no grace the role goes now, otherwise the grace starts now.
  if (ended === null) return grace === 0 ? { type: 'delete', why: 'the boost ended' } : { type: 'ended' };
  return ended + grace <= now ? { type: 'delete', why: 'the grace period after the boost is over' } : { type: 'none' };
}

// ── Table ─────────────────────────────────────────────────────────────────────

const keyOf = (guildId, userId) => ({ guildId_userId: { guildId, userId } });

/** The booster role row of a member, or null. */
export function loadRow(guildId, userId) {
  return prisma.boosterRole.findUnique({ where: keyOf(guildId, userId) });
}

/** Every booster role row of a guild, oldest first (Discord allows 250 roles, so this is bounded). */
export function listRows(guildId) {
  return prisma.boosterRole.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, take: 500 });
}

export function countRows(guildId) {
  return prisma.boosterRole.count({ where: { guildId } });
}

/** Deletes a member's row; returns how many went. Never fails on a missing row. */
export async function deleteRow(guildId, userId) {
  const res = await prisma.boosterRole.deleteMany({ where: { guildId, userId } });
  return res?.count ?? 0;
}

async function deleteRowsByRole(guildId, roleId) {
  const res = await prisma.boosterRole.deleteMany({ where: { guildId, roleId } });
  return res?.count ?? 0;
}

/** Sets `boostEndedAt` (only when it is not set yet). */
export function markEnded(guildId, userId, when = new Date()) {
  return prisma.boosterRole.updateMany({ where: { guildId, userId, boostEndedAt: null }, data: { boostEndedAt: when } });
}

/** Clears `boostEndedAt` (the member boosts again; the role was kept). */
export function clearEnded(guildId, userId) {
  return prisma.boosterRole.updateMany({ where: { guildId, userId, boostEndedAt: { not: null } }, data: { boostEndedAt: null } });
}

// ── Scheduling ────────────────────────────────────────────────────────────────

const sweepKey = (guildId) => `boosts.sweep:${guildId}`;

/**
 * Makes sure the recurring sweep exists. A live job is left alone ('keep'); a missing, DONE or FAILED one is created again.
 * Called when the first role is made, when a boost ends with a grace period, on guildReady when rows exist and by the
 * admin subcommands that list rows. Never throws.
 */
export async function ensureSweepJob(guildId, runAt = new Date(Date.now() + BOOST.SWEEP_EVERY_MS)) {
  try {
    await scheduleJob({
      guildId, type: BOOST.JOB_TYPE, runAt, payload: {}, dedupeKey: sweepKey(guildId), onConflict: 'keep',
    });
    return true;
  } catch (err) {
    logger.error(`boosts: could not schedule the sweep of guild ${guildId}`, err);
    return false;
  }
}

/** The state of a guild's sweep job (tests and /boost config): the job row or null. */
export const sweepJob = (guildId) => getJob(sweepKey(guildId));

// ── Discord calls ─────────────────────────────────────────────────────────────

/**
 * Deletes a role by id. 'deleted', 'gone' (it did not exist: Unknown Role), 'forbidden' (Missing Permissions) or 'error'.
 * Never throws; a role id that is @everyone is never touched.
 */
export async function removeRole(guild, roleId, why) {
  if (!roleId || roleId === guild.id) return 'gone';
  try {
    await guild.roles.delete(roleId, why);
    return 'deleted';
  } catch (err) {
    if (err?.code === 10011) return 'gone';
    if (err?.code === 50013) return 'forbidden';
    logger.warn(`[boosts] ${guild.id}: could not delete role ${roleId}: ${err?.message}`);
    return 'error';
  }
}

/**
 * Deletes a booster's role and then their row. A role that is already gone counts as deleted; when the role cannot be
 * deleted (no permission, API error) the row is kept, so the sweep tries again. Never throws.
 * @returns {Promise<'deleted'|'gone'|'forbidden'|'error'>}
 */
export async function removeBoosterRole(guild, row, why) {
  const res = await removeRole(guild, row.roleId, why);
  if (res !== 'deleted' && res !== 'gone') {
    if (res === 'forbidden' && warnBlocked(guild.id)) {
      logger.warn(`[boosts] ${guild.id}: cannot delete booster roles, the bot lacks Manage Roles or they are above its highest role`);
    }
    return res;
  }
  try {
    await deleteRow(guild.id, row.userId);
  } catch (err) {
    logger.error(`[boosts] ${guild.id}: could not delete the row of ${row.userId}`, err);
    return 'error';
  }
  return res;
}

/** The server's Nitro Booster role (made by Discord, tagged `premiumSubscriberRole`), or null. */
export function nitroBoosterRole(guild) {
  const direct = guild.roles?.premiumSubscriberRole;
  if (direct) return direct;
  for (const role of guild.roles?.cache?.values?.() ?? []) if (role.tags?.premiumSubscriberRole) return role;
  return null;
}

/**
 * Where a new booster role goes: just below the anchor role when one is set; otherwise just ABOVE the Nitro Booster role (its
 * colour shows over the boost role, and it stays low in the hierarchy so moderators can still act on boosters); with neither,
 * at the bottom (position 1). Never at or above the bot's highest role. Needs Manage Roles and a highest role of position 2
 * or more. `target` is the position to move the new role to, read from FRESH positions (after the create, see moveToSpot).
 * @returns {{ ok: true, kind: 'anchor'|'nitro'|'bottom', base: object|null, target: number }
 *   | { ok: false, code: 'BOT_PERMS'|'BOT_LOW'|'ANCHOR_HIGH' }}
 */
export function placementFor(guild, settings) {
  const me = guild.members?.me;
  if (!me?.permissions?.has(PermissionFlagsBits.ManageRoles)) return { ok: false, code: 'BOT_PERMS' };
  const top = me.roles?.highest;
  if (!top || top.position < 2) return { ok: false, code: 'BOT_LOW' };
  const anchorId = settings?.boostRoleAnchorId ?? null;
  const anchor = anchorId && anchorId !== guild.id ? guild.roles.cache.get(anchorId) ?? null : null;
  if (anchor && anchor.position > top.position) return { ok: false, code: 'ANCHOR_HIGH' };
  if (anchor) return { ok: true, kind: 'anchor', base: anchor, target: anchor.position - 1 };
  const nitro = nitroBoosterRole(guild);
  if (nitro) return { ok: true, kind: 'nitro', base: nitro, target: Math.min(nitro.position, top.position - 1) };
  return { ok: true, kind: 'bottom', base: null, target: 1 };
}

// Moves a freshly created role to its spot (placementFor). Roles are created at the bottom (position 1) and Discord shifts the
// rest up by one, so the roles are fetched first (the cached positions are stale). Moving the role out of slot 1 to `target`
// shifts the roles in between down by one: target = anchor.position - 1 puts it right below the anchor, target =
// nitro.position right above the Nitro Booster role. Returns false when it could not be moved.
async function moveToSpot(guild, role, settings, why) {
  try {
    await guild.roles.fetch();
  } catch (err) {
    logger.debug(`[boosts] ${guild.id}: could not refresh the roles before moving: ${err?.message}`);
  }
  const spot = placementFor(guild, settings);
  if (!spot.ok) return false;
  const target = spot.target;
  if (target <= 1) return true; // it stays at the bottom
  try {
    await role.setPosition(target, { reason: why });
    return true;
  } catch (err) {
    logger.warn(`[boosts] ${guild.id}: could not move booster role ${role.id}: ${err?.message}`);
    return false;
  }
}

// ── Cooldown ──────────────────────────────────────────────────────────────────

const lastEdit = new Map(); // `${guildId}:${userId}` -> epoch ms

/** Milliseconds until the member may create or edit again (0 = now). */
export function cooldownLeftMs(guildId, userId, now = Date.now()) {
  const at = lastEdit.get(`${guildId}:${userId}`);
  return at === undefined ? 0 : Math.max(0, at + BOOST.EDIT_COOLDOWN_MS - now);
}

/** Starts the member's cooldown (call when a Discord call is about to be made, never for a refusal). */
export function noteEdit(guildId, userId, now = Date.now()) {
  if (lastEdit.size >= BOOST.COOLDOWN_MAX) {
    for (const [k, t] of lastEdit) if (now - t >= BOOST.EDIT_COOLDOWN_MS) lastEdit.delete(k);
    if (lastEdit.size >= BOOST.COOLDOWN_MAX) lastEdit.delete(lastEdit.keys().next().value);
  }
  lastEdit.set(`${guildId}:${userId}`, now);
}

/** Forgets every cooldown (tests). */
export function resetCooldowns() {
  lastEdit.clear();
}

// ── Icons ─────────────────────────────────────────────────────────────────────

const isPng = (b) => b.length > 8 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47;
const isJpeg = (b) => b.length > 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff;
const ICON_TYPES = new Set(['image/png', 'image/jpeg', 'image/jpg']);
const iconKb = () => Math.floor(LIMITS.BOOSTER_ROLE_ICON_BYTES / 1000);

class IconProblem extends Error {}

async function defaultIconFetch(url) {
  const res = await safeFetch(url, { maxBytes: LIMITS.BOOSTER_ROLE_ICON_BYTES, timeoutMs: BOOST.ICON_TIMEOUT_MS, headers: { accept: 'image/png,image/jpeg' } });
  if (!res.ok) throw new IconProblem('The image could not be downloaded.');
  return res.buffer;
}
let iconFetch = defaultIconFetch;

/** Test seam: replaces the download (url -> Buffer). Called with no argument it restores the real one. */
export function __setIconFetch(fn) {
  iconFetch = fn ?? defaultIconFetch;
}

// The attachment is checked before it is downloaded (type, size), the bytes after (PNG or JPEG signature, size).
function iconAttachmentProblem(att) {
  const type = String(att?.contentType ?? '').split(';')[0].trim().toLowerCase();
  if (!att?.url || !ICON_TYPES.has(type)) return `The icon must be a PNG or JPEG image of at most ${iconKb()} KB.`;
  if (Number(att.size) > LIMITS.BOOSTER_ROLE_ICON_BYTES) return `The icon is too big: at most ${iconKb()} KB.`;
  return null;
}

async function fetchIcon(att) {
  let buf;
  try {
    buf = await iconFetch(att.url);
  } catch (err) {
    throw new IconProblem(err instanceof IconProblem ? err.message
      : err?.code === 'TOO_LARGE' ? `The icon is too big: at most ${iconKb()} KB.` : 'The image could not be downloaded.');
  }
  if (!Buffer.isBuffer(buf) || buf.length === 0 || buf.length > LIMITS.BOOSTER_ROLE_ICON_BYTES || !(isPng(buf) || isJpeg(buf))) {
    throw new IconProblem(`The icon must be a PNG or JPEG image of at most ${iconKb()} KB.`);
  }
  return buf;
}

// ── Messages ──────────────────────────────────────────────────────────────────

/** "The module is off" in the router's wording. */
export const moduleOffEmbed = () => warnEmbed('Module Disabled',
  `The **${LABEL}** module is turned off on this server. An admin can enable it with \`/module enable boosts\`.`);

function discordText(err) {
  switch (err?.code) {
    case 50013: return 'I am missing permissions: I need **Manage Roles** and my highest role must be above the booster role spot.';
    case 30005: return `This server has reached Discord's limit of ${BOOST.ROLES_PER_GUILD} roles.`;
    case 50035: return 'Discord rejected the role details (for example the icon image).';
    default: return 'Discord refused the change. Please try again in a moment.';
  }
}

const PROBLEMS = Object.freeze({
  SETTINGS: () => errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.'),
  MODULE_OFF: moduleOffEmbed,
  ROLES_OFF: () => errorEmbed('Booster Roles Are Off', 'An admin has not turned on booster roles on this server yet.'),
  NOT_BOOSTING: () => errorEmbed('Boosters Only', 'You need to be boosting this server to do that.'),
  ALREADY: () => errorEmbed('You Already Have A Role', 'Change it with `/boost role edit`, or remove it with `/boost role delete`.'),
  NO_ROLE: () => errorEmbed('No Booster Role', 'You do not have a booster role. Make one with `/boost role create`.'),
  COOLDOWN: (d) => errorEmbed('Slow Down', `You can change your role again in ${Math.max(1, Math.ceil((d?.ms ?? 0) / 1000))} seconds.`),
  BAD_NAME: (d) => errorEmbed('Invalid Name', NAME_REASONS[d?.reason] ?? NAME_REASONS.empty),
  BAD_COLOR: () => errorEmbed('Invalid Colour', 'Use a hex colour such as `#ff8800`.'),
  BAD_EMOJI: () => errorEmbed('Invalid Icon', 'Use one standard emoji. Custom emojis cannot be role icons; upload an image instead.'),
  BAD_ICON: (d) => errorEmbed('Invalid Icon Image', d?.text ?? 'The icon could not be used.'),
  ICON_BOTH: () => errorEmbed('Pick One Icon', 'Use either `icon_emoji` or `icon_image`, not both.'),
  NO_ROLE_ICONS: () => errorEmbed('Role Icons Locked', 'This server has not unlocked role icons yet (they need Boost level 2).'),
  NOTHING: () => errorEmbed('Nothing To Change', 'Give at least one of `name`, `color`, `icon_emoji` or `icon_image`.'),
  BOT_PERMS: () => errorEmbed('Missing Permissions', 'I need the **Manage Roles** permission to make booster roles. Ask an admin.'),
  BOT_LOW: () => errorEmbed('Cannot Place The Role', 'My highest role is too low to put a role below it. An admin must move my role up.'),
  ANCHOR_HIGH: () => errorEmbed('Cannot Place The Role', 'The role booster roles go below is above my highest role. An admin must fix `/boost config`.'),
  ROLE_LIMIT: () => errorEmbed('Role Limit Reached', `This server has reached Discord's limit of ${BOOST.ROLES_PER_GUILD} roles.`),
  BOT_ROLE: () => errorEmbed('Cannot Edit The Role', 'Your booster role is above my highest role, so I cannot change it. Ask an admin.'),
  DISCORD: (d) => errorEmbed('Discord Refused', d?.text ?? 'Discord refused the change.'),
  FAILED: () => errorEmbed('Could Not Save', 'Something went wrong. Please try again in a moment.'),
});

/** The error embed for a `{ ok: false, code, detail }` result. */
export function problemEmbed(result) {
  return (PROBLEMS[result?.code] ?? PROBLEMS.FAILED)(result?.detail);
}

const fail = (code, detail) => ({ ok: false, code, detail });

/**
 * Why a member may not use `/boost role ...` right now, or null: module off, booster roles off, not boosting.
 * @param {{ needBoosting?: boolean }} [opts]
 * @returns {{ ok: false, code: string }|null}
 */
export function memberProblem(settings, member, { needBoosting = true } = {}) {
  if (!settings) return fail('SETTINGS');
  if (!isModuleEnabled(settings, 'boosts')) return fail('MODULE_OFF');
  if (settings.boostCustomRoles !== true) return fail('ROLES_OFF');
  if (needBoosting && !isBoosting(member)) return fail('NOT_BOOSTING');
  return null;
}

// ── Looks (name, colour, icon) ────────────────────────────────────────────────

// Checks what the member asked for. Nothing here talks to Discord or spends the cooldown.
function prepareLook({ guild, name, color, iconEmoji, iconImage, exceptRoleId = null, requireName = false }) {
  const look = {};
  if (name !== null && name !== undefined) {
    const checked = validateRoleName(name, guild, { exceptRoleId });
    if (!checked.ok) return fail('BAD_NAME', { reason: checked.reason });
    look.name = checked.name;
  } else if (requireName) {
    return fail('BAD_NAME', { reason: 'empty' });
  }
  const colour = parseColor(color);
  if (!colour.ok) return fail('BAD_COLOR');
  if (colour.value !== null) look.color = colour.value;

  const wantsEmoji = iconEmoji !== null && iconEmoji !== undefined && String(iconEmoji).trim() !== '';
  const wantsImage = Boolean(iconImage);
  if (wantsEmoji && wantsImage) return fail('ICON_BOTH');
  if (wantsEmoji || wantsImage) {
    const features = guild?.features ?? [];
    if (!(Array.isArray(features) ? features : [...features]).includes('ROLE_ICONS')) return fail('NO_ROLE_ICONS');
    if (wantsEmoji) {
      const e = parseEmoji(iconEmoji);
      if (!e.ok) return fail('BAD_EMOJI');
      look.unicodeEmoji = e.emoji;
    } else {
      const problem = iconAttachmentProblem(iconImage);
      if (problem) return fail('BAD_ICON', { text: problem });
      look.iconAttachment = iconImage;
    }
  }
  return { ok: true, look };
}

async function resolveIcon(look) {
  if (!look.iconAttachment) return null;
  try {
    return await fetchIcon(look.iconAttachment);
  } catch (err) {
    return fail('BAD_ICON', { text: err instanceof IconProblem ? err.message : 'The image could not be downloaded.' });
  }
}

const colorsOf = (look) => (look.color === undefined ? {} : { colors: { primaryColor: look.color } });

// ── Create / edit / delete (member) ───────────────────────────────────────────

/**
 * `/boost role create`. The caller has run memberProblem. Checks, then: create the role (no permissions, not hoisted, not
 * mentionable), move it to its spot (placementFor), give it to the member, save the row. A failure after the create deletes
 * the role.
 * @returns {Promise<{ ok: true, role: object, moved: boolean } | { ok: false, code: string, detail?: object }>}
 */
export function createBoosterRole({ guild, member, settings, name, color = null, iconEmoji = null, iconImage = null }) {
  return runLocked(guild.id, member.id, async () => {
    try {
      const left = cooldownLeftMs(guild.id, member.id);
      if (left > 0) return fail('COOLDOWN', { ms: left });
      const existing = await loadRow(guild.id, member.id);
      if (existing) {
        if (guild.roles.cache.has(existing.roleId)) return fail('ALREADY');
        await deleteRow(guild.id, member.id); // the role was deleted behind our back: start over
      }
      const prep = prepareLook({ guild, name, color, iconEmoji, iconImage, requireName: true });
      if (!prep.ok) return prep;
      const spot = placementFor(guild, settings);
      if (!spot.ok) return fail(spot.code);
      if (guild.roles.cache.size >= BOOST.ROLES_PER_GUILD) return fail('ROLE_LIMIT');
      const icon = await resolveIcon(prep.look);
      if (icon && icon.ok === false) return icon;

      noteEdit(guild.id, member.id);
      let role;
      try {
        role = await guild.roles.create({
          name: prep.look.name,
          ...colorsOf(prep.look),
          hoist: false,
          mentionable: false,
          permissions: 0n,
          ...(prep.look.unicodeEmoji ? { unicodeEmoji: prep.look.unicodeEmoji } : {}),
          ...(icon ? { icon } : {}),
          reason: reason(`booster role made by ${member.id}`),
        });
      } catch (err) {
        logger.warn(`[boosts] ${guild.id}: could not create a role for ${member.id}: ${err?.message}`);
        return fail('DISCORD', { text: discordText(err) });
      }

      let moved = false;
      try {
        moved = await moveToSpot(guild, role, settings, reason('placing a booster role'));
        await member.roles.add(role, reason('booster role of the member'));
        await ensureGuildSettings(guild.id);
        await prisma.boosterRole.create({ data: { guildId: guild.id, userId: member.id, roleId: role.id } });
      } catch (err) {
        await removeRole(guild, role.id, reason('cleaning up a booster role that could not be saved'));
        if (err?.code === 'P2002') return fail('ALREADY');
        logger.error(`[boosts] ${guild.id}: could not finish the booster role of ${member.id}`, err);
        return err?.code && Number.isInteger(err.code) ? fail('DISCORD', { text: discordText(err) }) : fail('FAILED');
      }
      await ensureSweepJob(guild.id);
      logger.info(`[boosts] ${guild.id}: ${member.id} made booster role ${role.id}`);
      return { ok: true, role, moved };
    } catch (err) {
      logger.error(`[boosts] ${guild.id}: create failed for ${member.id}`, err);
      return fail('FAILED');
    }
  });
}

/**
 * `/boost role edit`: name, colour or icon of the member's own role. Needs at least one of them. The member's own current
 * name does not collide with itself. A role the member lost is given back.
 * @returns {Promise<{ ok: true, role: object, changed: string[] } | { ok: false, code: string, detail?: object }>}
 */
export function editBoosterRole({ guild, member, name = null, color = null, iconEmoji = null, iconImage = null }) {
  return runLocked(guild.id, member.id, async () => {
    try {
      const left = cooldownLeftMs(guild.id, member.id);
      if (left > 0) return fail('COOLDOWN', { ms: left });
      const row = await loadRow(guild.id, member.id);
      if (!row) return fail('NO_ROLE');
      const role = guild.roles.cache.get(row.roleId);
      if (!role) {
        await deleteRow(guild.id, member.id);
        return fail('NO_ROLE');
      }
      const asked = [name, color, iconEmoji, iconImage].some((v) => v !== null && v !== undefined && String(v).trim?.() !== '');
      if (!asked) return fail('NOTHING');
      const prep = prepareLook({ guild, name, color, iconEmoji, iconImage, exceptRoleId: role.id });
      if (!prep.ok) return prep;
      if (role.editable === false) return fail('BOT_ROLE');
      const icon = await resolveIcon(prep.look);
      if (icon && icon.ok === false) return icon;

      noteEdit(guild.id, member.id);
      const { look } = prep;
      try {
        await role.edit({
          ...(look.name !== undefined ? { name: look.name } : {}),
          ...colorsOf(look),
          ...(look.unicodeEmoji ? { unicodeEmoji: look.unicodeEmoji } : {}),
          ...(icon ? { icon } : {}),
          reason: reason(`booster role edited by ${member.id}`),
        });
      } catch (err) {
        logger.warn(`[boosts] ${guild.id}: could not edit role ${role.id}: ${err?.message}`);
        return fail('DISCORD', { text: discordText(err) });
      }
      if (!member.roles?.cache?.has(role.id)) {
        await member.roles.add(role, reason('booster role of the member')).catch((err) => {
          logger.warn(`[boosts] ${guild.id}: could not give ${role.id} back to ${member.id}: ${err?.message}`);
        });
      }
      const changed = [look.name !== undefined && 'name', look.color !== undefined && 'colour',
        (look.unicodeEmoji || icon) && 'icon'].filter(Boolean);
      return { ok: true, role, changed };
    } catch (err) {
      logger.error(`[boosts] ${guild.id}: edit failed for ${member.id}`, err);
      return fail('FAILED');
    }
  });
}

/**
 * `/boost role delete`: the member removes their own role (also after their boost ended, while the grace period runs).
 * @returns {Promise<{ ok: true } | { ok: false, code: string, detail?: object }>}
 */
export function deleteOwnRole({ guild, member }) {
  return runLocked(guild.id, member.id, async () => {
    try {
      const row = await loadRow(guild.id, member.id);
      if (!row) return fail('NO_ROLE');
      const res = await removeBoosterRole(guild, row, reason(`booster role deleted by ${member.id}`));
      if (res === 'forbidden') return fail('BOT_ROLE');
      if (res === 'error') return fail('FAILED');
      return { ok: true };
    } catch (err) {
      logger.error(`[boosts] ${guild.id}: delete failed for ${member.id}`, err);
      return fail('FAILED');
    }
  });
}

/**
 * `/boost remove` (admin): deletes a member's booster role and row.
 * @returns {Promise<{ ok: true, roleId: string } | { ok: false, code: 'NO_ROLE'|'BOT_ROLE'|'FAILED' }>}
 */
export function removeMemberRole({ guild, userId, actorId }) {
  return runLocked(guild.id, userId, async () => {
    try {
      const row = await loadRow(guild.id, userId);
      if (!row) return fail('NO_ROLE');
      const res = await removeBoosterRole(guild, row, reason(`booster role removed by ${actorId}`));
      if (res === 'forbidden') return fail('BOT_ROLE');
      if (res === 'error') return fail('FAILED');
      return { ok: true, roleId: row.roleId };
    } catch (err) {
      logger.error(`[boosts] ${guild.id}: remove failed for ${userId}`, err);
      return fail('FAILED');
    }
  });
}

// ── Thank-you ─────────────────────────────────────────────────────────────────

const warnChannel = createThrottle(3_600_000);

/**
 * Posts the thank-you for a member in `boostChannelId`. Pings only that member (a preview pings nobody). Never throws.
 * @param {{ preview?: boolean }} [opts] a preview carries a label line and is posted for `/boost test`
 * @returns {Promise<{ ok: true, channelId: string } | { ok: false, reason: 'no-channel'|'unusable'|'failed', text?: string }>}
 */
export async function postThankYou(guild, member, settings, { preview = false } = {}) {
  const channelId = settings?.boostChannelId ?? null;
  if (!channelId) return { ok: false, reason: 'no-channel' };
  const channel = guild.channels?.cache?.get(channelId) ?? null;
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  if (!usable.ok) {
    if (!preview && warnChannel(guild.id)) logger.warn(`[boosts] ${guild.id}: cannot post the boost thank-you: ${usable.reason}`);
    return { ok: false, reason: 'unusable', text: usable.reason };
  }
  const head = preview ? '🧪 **Preview of the boost thank-you** (nobody was pinged)\n' : '';
  const content = head + renderThankYou(settings, member, guild, { maxLength: BOOST.MESSAGE_MAX - head.length });
  try {
    await channel.send({ content, allowedMentions: preview ? NO_MENTIONS : { parse: [], users: [member.id] } });
    return { ok: true, channelId };
  } catch (err) {
    if (!preview && warnChannel(guild.id)) logger.warn(`[boosts] ${guild.id}: could not post the boost thank-you: ${err?.message}`);
    return { ok: false, reason: 'failed', text: err?.message };
  }
}

// ── Hook bodies and filters ───────────────────────────────────────────────────

/**
 * guildMemberUpdate (sync): only a change of the boost state of a person is looked at. A partial old member (not cached,
 * `oldKnown` false) has no known `premiumSince`, so it never counts as a boost starting; the sweep repairs those.
 */
export function memberUpdateFilter(ctx) {
  if (ctx.oldKnown !== true) return false;
  const next = ctx.newMember;
  if (!next || next.user?.bot === true || !ctx.oldMember) return false;
  return isBoosting(ctx.oldMember) !== isBoosting(next);
}

/** guildMemberRemove (sync): people only. */
export function memberRemoveFilter(ctx) {
  return Boolean(ctx.user) && ctx.user.bot !== true;
}

/** channelDelete (sync): skipped once the memoised settings show the channel is not the thank-you channel. */
export function channelDeleteFilter(ctx) {
  const s = peekSettings(ctx.guildId);
  return !s || s.boostChannelId === ctx.channel?.id;
}

async function onBoostStart(guild, member, settings) {
  try {
    const cleared = await clearEnded(guild.id, member.id);
    if (cleared?.count) logger.info(`[boosts] ${guild.id}: ${member.id} boosts again, keeping their booster role`);
  } catch (err) {
    logger.warn(`[boosts] ${guild.id}: could not clear the end of the boost of ${member.id}: ${err?.message}`);
  }
  await postThankYou(guild, member, settings);
}

async function onBoostEnd(guild, member, settings) {
  await runLocked(guild.id, member.id, async () => {
    const row = await loadRow(guild.id, member.id);
    if (!row) return;
    if (!row.boostEndedAt) await markEnded(guild.id, member.id, new Date());
    if (graceMs(settings) === 0) {
      await removeBoosterRole(guild, row, reason('the boost ended'));
    } else {
      await ensureSweepJob(guild.id); // the grace period ends later; the sweep removes the role then
    }
  });
}

/** guildMemberUpdate: a boost started (thank-you, a returning booster keeps their role) or ended (grace period starts). */
export async function onMemberUpdate(ctx) {
  const kind = boostTransition(ctx.oldMember, ctx.newMember);
  if (!kind) return;
  const settings = await ctx.getSettings();
  if (!settings) return;
  try {
    if (kind === 'start') await onBoostStart(ctx.guild, ctx.newMember, settings);
    else await onBoostEnd(ctx.guild, ctx.newMember, settings);
  } catch (err) {
    logger.error(`[boosts] ${ctx.guildId}: boost ${kind} handling failed for ${ctx.newMember?.id}`, err);
  }
}

/** guildMemberRemove: the member's booster role and row go. */
export async function onMemberRemove(ctx) {
  const userId = ctx.user?.id;
  if (!userId) return;
  try {
    await runLocked(ctx.guildId, userId, async () => {
      const row = await loadRow(ctx.guildId, userId);
      if (row) await removeBoosterRole(ctx.guild, row, reason('the member left the server'));
    });
  } catch (err) {
    logger.error(`[boosts] ${ctx.guildId}: could not remove the booster role of ${userId} who left`, err);
  }
}

/** roleDelete (always): the row of a deleted booster role is dropped; a deleted anchor role is forgotten. */
export async function onRoleDelete(ctx) {
  const roleId = ctx.role?.id;
  if (!roleId) return;
  try {
    await deleteRowsByRole(ctx.guildId, roleId);
  } catch (err) {
    logger.warn(`[boosts] ${ctx.guildId}: could not drop the row of deleted role ${roleId}: ${err?.message}`);
  }
  try {
    const settings = await ctx.getSettings();
    if (settings?.boostRoleAnchorId === roleId) await updateSettings(ctx.guildId, { boostRoleAnchorId: null });
  } catch (err) {
    logger.warn(`[boosts] ${ctx.guildId}: could not clear the deleted anchor role: ${err?.message}`);
  }
}

/** channelDelete (always): a deleted thank-you channel is forgotten. */
export async function onChannelDelete(ctx) {
  const settings = await ctx.getSettings();
  if (!settings || settings.boostChannelId !== ctx.channel?.id) return;
  try {
    await updateSettings(ctx.guildId, { boostChannelId: null });
  } catch (err) {
    logger.warn(`[boosts] ${ctx.guildId}: could not clear the deleted thank-you channel: ${err?.message}`);
  }
}

/** guildReady: a guild with booster roles gets its sweep back (a live job is kept). */
export async function onGuildReady(ctx) {
  let n;
  try {
    n = await countRows(ctx.guildId);
  } catch (err) {
    logger.error(`[boosts] ${ctx.guildId}: could not count the booster roles on startup`, err);
    return;
  }
  if (n > 0) await ensureSweepJob(ctx.guildId, new Date(Date.now() + BOOST.STARTUP_DELAY_MS + Math.floor(Math.random() * BOOST.STARTUP_JITTER_MS)));
}

// ── The sweep ─────────────────────────────────────────────────────────────────

// Where the next run starts looking members up, per guild (in memory), so a long list is covered over several runs.
const lookupOffset = new Map();

/** Forgets the lookup offsets (tests). */
export function resetSweepState() {
  lookupOffset.clear();
}

async function lookupMember(guild, userId) {
  try {
    return await withTimeout(guild.members.fetch(userId), BOOST.LOOKUP_TIMEOUT_MS, 'boost sweep member lookup');
  } catch (err) {
    if (err?.code === 10007) return null; // Unknown Member: left the server
    logger.debug(`[boosts] ${guild.id}: could not look up ${userId}: ${err?.message}`);
    return undefined; // unknown: never act on it
  }
}

// Applies one row's plan under the member's lock, after reading the row and the member state again.
function applyPlan(guild, row, resolved, settings, now) {
  return runLocked(guild.id, row.userId, async () => {
    const fresh = await loadRow(guild.id, row.userId);
    if (!fresh) return 'none';
    const member = guild.members.cache.get(row.userId) ?? resolved;
    const plan = planSweep(fresh, member, settings, now);
    if (plan.type === 'ended') {
      await markEnded(guild.id, row.userId, new Date(now));
      return 'ended';
    }
    if (plan.type === 'resumed') {
      await clearEnded(guild.id, row.userId);
      return 'resumed';
    }
    if (plan.type === 'delete') {
      const res = await removeBoosterRole(guild, fresh, reason(plan.why));
      return res === 'deleted' || res === 'gone' ? 'deleted' : 'failed';
    }
    return 'none';
  });
}

/**
 * One pass over a guild's rows. Members come from the cache; the ones that are not cached (a member who left is not) are
 * looked up one by one, at most LOOKUPS_PER_RUN per run, and deletions are capped at DELETES_PER_RUN, so a run is bounded.
 * `more` says the next run should come soon because something was left over.
 * @returns {Promise<{ deleted: number, ended: number, resumed: number, failed: number, more: boolean }>}
 */
export async function sweepGuild(guild, rows, settings, { now = Date.now(), budgetMs = BOOST.SWEEP_BUDGET_MS } = {}) {
  const out = { deleted: 0, ended: 0, resumed: 0, failed: 0, more: false };
  const deadline = Date.now() + budgetMs;
  const late = () => Date.now() >= deadline;
  const resolved = new Map(); // userId -> GuildMember | null (left)
  const uncached = [];
  for (const row of rows) {
    const cached = guild.members.cache.get(row.userId);
    if (cached) resolved.set(row.userId, cached);
    else uncached.push(row.userId);
  }
  if (uncached.length) {
    const start = (lookupOffset.get(guild.id) ?? 0) % uncached.length;
    const order = [...uncached.slice(start), ...uncached.slice(0, start)];
    const todo = order.slice(0, BOOST.LOOKUPS_PER_RUN);
    let looked = 0;
    for (const userId of todo) {
      if (late()) break;
      const member = await lookupMember(guild, userId);
      looked += 1;
      if (member !== undefined) resolved.set(userId, member);
    }
    if (order.length > looked) {
      out.more = true;
      lookupOffset.set(guild.id, start + looked);
    } else {
      lookupOffset.delete(guild.id);
    }
  }

  let deletes = 0;
  for (const row of rows) {
    const member = resolved.has(row.userId) ? resolved.get(row.userId) : undefined;
    const plan = planSweep(row, member, settings, now);
    if (plan.type === 'none' || plan.type === 'skip') continue;
    if (late()) {
      out.more = true;
      break;
    }
    if (plan.type === 'delete') {
      if (deletes >= BOOST.DELETES_PER_RUN) {
        out.more = true;
        continue;
      }
      deletes += 1;
    }
    try {
      const result = await applyPlan(guild, row, member, settings, now);
      if (result === 'deleted') out.deleted += 1;
      else if (result === 'ended') out.ended += 1;
      else if (result === 'resumed') out.resumed += 1;
      else if (result === 'failed') out.failed += 1;
    } catch (err) {
      out.failed += 1;
      logger.error(`[boosts] ${guild.id}: sweep failed for ${row.userId}`, err);
    }
  }
  if (out.deleted || out.ended || out.resumed) {
    logger.info(`[boosts] ${guild.id}: sweep removed ${out.deleted} role(s), ${out.ended} boost(s) ended, ${out.resumed} resumed`);
  }
  return out;
}

/**
 * The boosts.sweep job body. Never throws, so the recurring job cannot go FAILED. It ends itself (returns nothing) when the
 * guild has no rows. It cleans up whether or not the module is on: a role of a member who stopped boosting or left should
 * not linger just because an admin switched the module off.
 * @param {{ guildId: string }} job
 * @param {{ guild: object }} ctx
 */
export async function sweepJobHandler(job, ctx) {
  const now = Date.now();
  try {
    const guild = ctx.guild;
    if (!guild?.available) return { nextRunAt: new Date(now + BOOST.SWEEP_RETRY_MS) };
    const rows = await listRows(job.guildId);
    if (rows.length === 0) return undefined;
    const settings = await getSettingsSafe(job.guildId);
    if (!settings) return { nextRunAt: new Date(now + 2 * 60_000) };
    const out = await sweepGuild(guild, rows, settings, { now });
    return { nextRunAt: new Date(Date.now() + (out.more ? BOOST.SWEEP_MORE_MS : BOOST.SWEEP_EVERY_MS)) };
  } catch (err) {
    logger.error(`boosts.sweep failed for guild ${job.guildId}`, err);
    return { nextRunAt: new Date(Date.now() + BOOST.SWEEP_RETRY_MS) };
  }
}
