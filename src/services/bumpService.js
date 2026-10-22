// Bump Reminders (beyond MEE6): the bot watches for a successful /bump on a bump site (today Disboard), remembers when the
// server can be bumped again, reminds the server (optionally pinging a role) when the cooldown ends, thanks the bumper and
// keeps a leaderboard of who bumps. This file holds the rules: the site table and its pure success check, the bumper lookup,
// the state / count writes, the `bump.remind` job and the cleanup hooks. src/plugins/bumps.js declares the hooks and the job;
// src/commands/community/bumps.js is the command.
import { Prisma } from '@prisma/client';
import prisma, { sqlUtc } from '../database/client.js';
import { COIN_MAX, XP_MAX } from '../config/constants.js';
import { MODULES } from '../config/modules.js';
import {
  ensureGuildSettings, getSettings, isModuleEnabled, peekSettings, updateSettings,
} from './settingsService.js';
import { grant } from './rewards.js';
import { PermanentJobError, scheduleJob } from './scheduler.js';
import { createKeyedMutex, createThrottle } from '../utils/async.js';
import { warnEmbed } from '../utils/embedBuilder.js';
import { checkChannelUsable, pingableRoleIds } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';
import { renderPlaceholders } from '../utils/placeholderParser.js';

// ── Constants ─────────────────────────────────────────────────────────────────

const HOUR_MS = 60 * 60_000;

/** The Int clamp of BumpCount.count. */
export const BUMP_COUNT_MAX = XP_MAX;
/** Rows on `/bumps leaderboard`. */
export const LEADERBOARD_SIZE = 10;
/** A reminder job that runs this much before `nextBumpAt` (clock slack) still sends; earlier ones reschedule themselves. */
const EARLY_SLACK_MS = 5_000;
/** Message ids remembered so one bump reply is never processed twice. */
const SEEN_MAX = 500;
/** Discord errors that no retry fixes: unknown channel, missing access, missing permissions, archived thread. */
const PERMANENT_CODES = new Set([10003, 50001, 50013, 50083]);
const MESSAGE_MAX = 1900;

/** The texts used while a message column is null. Exact copies of DEFAULT_TEXTS in src/dashboard/fields.js. */
export const DEFAULT_REMINDER_MESSAGE = '📣 The server can be bumped again! Use `/bump` to keep us near the top of Disboard.';
export const DEFAULT_THANKS_MESSAGE = '📣 Thanks for bumping, {user}! The next bump is possible {next}.';

// ── The bump sites ────────────────────────────────────────────────────────────

// Disboard answers `/bump` with an interaction reply: an embed whose description reads "Bump done! :thumbsup:" and whose
// image is bot-command-image-bump. The failure reply ("Please wait another N minutes until the server can be bumped") is
// never a success. If the live shape ever differs, this one function is the only place to fix.
const DONE_RE = /bump done/i;
const WAIT_RE = /please wait|wait another|until the server can be bumped/i;
const IMAGE_MARK = 'bot-command-image-bump';

const embedDescription = (e) => String(e?.description ?? e?.data?.description ?? '');
const embedImageUrl = (e) => String(e?.image?.url ?? e?.data?.image?.url ?? '');

/**
 * Pure: whether a Disboard message is the answer to a successful bump.
 * @param {{ embeds?: Array<object> }|null} message
 * @returns {boolean}
 */
export function isDisboardSuccess(message) {
  const embeds = Array.isArray(message?.embeds) ? message.embeds : [];
  for (const e of embeds) {
    const text = embedDescription(e);
    if (WAIT_RE.test(text)) return false;
    if (DONE_RE.test(text) || embedImageUrl(e).includes(IMAGE_MARK)) return true;
  }
  return false;
}

/** Site key -> { botId, name, cooldownMs, isSuccess(message) }. Add a bump site here and nowhere else. */
export const BUMP_SITES = Object.freeze({
  disboard: Object.freeze({
    botId: '302050872383242240',
    name: 'Disboard',
    cooldownMs: 2 * HOUR_MS,
    isSuccess: isDisboardSuccess,
  }),
});

const SITE_BY_BOT = new Map(Object.entries(BUMP_SITES).map(([key, site]) => [site.botId, key]));

/** @returns {string|null} the site key whose bot has this user id */
export const siteKeyForBot = (userId) => SITE_BY_BOT.get(String(userId)) ?? null;

/**
 * The member who ran the bump command (interaction metadata, else the deprecated interaction); null when unknown or a bot.
 * @param {object|null} message
 * @returns {import('discord.js').User|null}
 */
export function bumperOf(message) {
  const user = message?.interactionMetadata?.user ?? message?.interaction?.user ?? null;
  return user && !user.bot ? user : null;
}

/** The sync hook filter: only messages by a known bump site's bot enter the plugin (every bot message in every server arrives). */
export const isBumpSiteMessage = (ctx) => SITE_BY_BOT.has(ctx?.message?.author?.id);

// ── Raw SQL (one entry per statement, with sample parameters) ─────────────────

const incrementCountSql = (guildId, userId, at) => Prisma.sql`
  INSERT INTO "BumpCount" ("guildId", "userId", "count", "lastBumpAt")
  VALUES (${guildId}, ${userId}, 1, ${sqlUtc(at)})
  ON CONFLICT ("guildId", "userId") DO UPDATE
  SET "count" = LEAST("BumpCount"."count"::bigint + 1, ${BUMP_COUNT_MAX}::bigint)::int, "lastBumpAt" = ${sqlUtc(at)}
  RETURNING "count"`;

export const RAW_SQL = Object.freeze([
  { name: 'bumps.incrementCount', build: () => incrementCountSql('0', '0', new Date(0)) },
]);

// ── Test seams ────────────────────────────────────────────────────────────────

let scheduleImpl = scheduleJob;
/** Replaces scheduleJob (tests); no argument restores the real one. */
export function __setScheduleImpl(impl) {
  scheduleImpl = impl ?? scheduleJob;
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/** The router's wording for a member command used while the module is off. */
export function moduleOffEmbed() {
  return warnEmbed('Module Disabled', `The **${MODULES.bumps.label}** module is turned off on this server. An admin can enable it with \`/module enable bumps\`.`);
}

export const reminderDedupeKey = (guildId, site) => `bump.remind:${guildId}:${site}`;

const unix = (date) => Math.floor(new Date(date).getTime() / 1000);

const seenMessages = new Set();
/** Marks a message id as processed; false when it already was. Bounded (oldest forgotten first). */
function markSeen(messageId) {
  if (!messageId || seenMessages.has(messageId)) return false;
  seenMessages.add(messageId);
  if (seenMessages.size > SEEN_MAX) seenMessages.delete(seenMessages.values().next().value);
  return true;
}
/** Forgets the processed message ids (tests). */
export function __resetSeen() {
  seenMessages.clear();
}

// One guild's bumps on one site run one at a time (a guild lives on one shard).
const lock = createKeyedMutex();
// A warning about an unusable reminder channel at most once an hour per guild.
const warnOncePerHour = createThrottle(HOUR_MS);

function warnLimited(guildId, text) {
  if (warnOncePerHour(guildId)) logger.warn(text);
  else logger.debug(text);
}

// ── State and counts ──────────────────────────────────────────────────────────

/**
 * Writes the guild's state for a site unless a bump at or after `at` is already recorded (a repeated message changes nothing).
 * @returns {Promise<boolean>} true when this bump is now the recorded one
 */
async function writeState({ guildId, site, at, bumperId, channelId, nextBumpAt }) {
  const data = { lastBumpAt: at, lastBumperId: bumperId, channelId, nextBumpAt, remindedAt: null };
  const updated = await prisma.bumpState.updateMany({ where: { guildId, site, lastBumpAt: { lt: at } }, data });
  if (updated.count > 0) return true;
  try {
    await prisma.bumpState.create({ data: { guildId, site, ...data } });
    return true;
  } catch (err) {
    if (err?.code === 'P2002') return false; // the row exists with a bump at or after `at`
    throw err;
  }
}

/** @returns {Promise<number|null>} the bumper's new total (clamped to BUMP_COUNT_MAX), or null when it could not be read */
async function incrementCount(guildId, userId, at) {
  const rows = await prisma.$queryRaw(incrementCountSql(guildId, userId, at));
  const count = Number(rows?.[0]?.count);
  return Number.isFinite(count) ? count : null;
}

async function scheduleReminder(guildId, site, runAt) {
  const job = {
    guildId, type: 'bump.remind', runAt, payload: { site }, dedupeKey: reminderDedupeKey(guildId, site), onConflict: 'replace',
  };
  try {
    await scheduleImpl(job);
  } catch (err) {
    logger.warn(`[bumps] ${guildId}: scheduling the reminder failed, retrying once (${err?.message})`);
    try {
      await scheduleImpl(job);
    } catch (err2) {
      // The bump itself is recorded; the thank-you and the count must still happen.
      logger.error(`[bumps] ${guildId}: the ${site} reminder could not be scheduled`, err2);
    }
  }
}

/**
 * Records one successful bump: the guild's state, the reminder job and the bumper's count. Serialised per guild and site.
 * @param {{ guildId: string, site: string, channelId: string, bumperId?: string|null, at?: Date }} args
 * @returns {Promise<{ recorded: boolean, count: number|null, nextBumpAt: Date|null }>} recorded false: an equal or newer bump was already known
 */
export function recordBump({ guildId, site, channelId, bumperId = null, at = new Date() }) {
  const def = BUMP_SITES[site];
  if (!def) return Promise.reject(new Error(`Unknown bump site "${site}"`));
  return lock.run(`${guildId}:${site}`, async () => {
    await ensureGuildSettings(guildId);
    const nextBumpAt = new Date(at.getTime() + def.cooldownMs);
    const recorded = await writeState({ guildId, site, at, bumperId, channelId, nextBumpAt });
    if (!recorded) return { recorded: false, count: null, nextBumpAt: null };
    // The reminder first: a failing count must never cost the server its reminder.
    await scheduleReminder(guildId, site, nextBumpAt);
    let count = null;
    if (bumperId) {
      try {
        count = await incrementCount(guildId, bumperId, at);
      } catch (err) {
        logger.error(`[bumps] ${guildId}: could not count the bump of ${bumperId}`, err);
      }
    }
    return { recorded: true, count, nextBumpAt };
  });
}

// ── The hook ──────────────────────────────────────────────────────────────────

async function sendThanks({ channel, guild, bumper, settings, def, count, nextBumpAt }) {
  if (!settings.bumpThanks || !bumper || !channel?.send) return false;
  // checkChannelUsable asks for Send Messages in Threads when the bump was made in a thread.
  const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  if (!usable.ok) {
    logger.debug(`[bumps] ${guild.id}: no thank-you in ${channel.id} (${usable.missing.join(', ') || usable.reason})`);
    return false;
  }
  const text = renderPlaceholders(settings.bumpThanksMessage || DEFAULT_THANKS_MESSAGE, {
    user: bumper,
    member: guild.members?.cache?.get(bumper.id) ?? null,
    guild,
    channel,
    vars: { next: `<t:${unix(nextBumpAt)}:R>`, count: count ?? '?', site: def.name },
  }, { maxLength: MESSAGE_MAX });
  if (!text.trim()) return false;
  // Only the bumper can be pinged, whatever the admin's text contains.
  await channel.send({ content: text, allowedMentions: { parse: [], users: [bumper.id] } });
  return true;
}

async function rewardBumper({ guild, bumper, channel, settings }) {
  const amount = Math.min(Math.trunc(Number(settings.bumpRewardCoins) || 0), COIN_MAX);
  if (amount < 1 || !bumper) return;
  const res = await grant('coins', {
    guild, userId: bumper.id, amount, reason: 'Bumped the server', member: guild.members?.cache?.get(bumper.id) ?? null, channel,
  });
  // The economy being off or missing is normal; anything else is worth a trace.
  if (!res.ok && res.reason !== 'module-disabled' && res.reason !== 'unavailable') {
    logger.debug(`[bumps] ${guild.id}: the bump reward for ${bumper.id} failed (${res.reason})`);
  }
}

/**
 * botMessage hook: a bump site's answer to a successful bump is recorded, the bumper thanked and rewarded.
 * Never throws for Discord failures; a database failure while recording is left to the hook runner to log.
 * @param {object} ctx HookContext of botMessage
 */
export async function onBotMessage(ctx) {
  const { message, guild } = ctx;
  const site = siteKeyForBot(message?.author?.id);
  if (!site || !guild) return;
  const def = BUMP_SITES[site];
  let success = false;
  try {
    success = def.isSuccess(message) === true;
  } catch (err) {
    logger.warn(`[bumps] ${site}: the success check threw (${err?.message})`);
  }
  if (!success || !markSeen(message.id)) return;

  const settings = await ctx.getSettings();
  if (!settings || !isModuleEnabled(settings, 'bumps')) return;

  const bumper = bumperOf(message);
  const at = new Date(Math.min(Number(message.createdTimestamp) || Date.now(), Date.now()));
  const out = await recordBump({ guildId: guild.id, site, channelId: message.channelId ?? ctx.channel?.id, bumperId: bumper?.id ?? null, at });
  if (!out.recorded) return;
  logger.info(`[bumps] ${guild.id}: ${site} bump by ${bumper?.id ?? 'an unknown member'}, next at ${out.nextBumpAt.toISOString()}`);

  const channel = ctx.channel ?? message.channel;
  try {
    await sendThanks({ channel, guild, bumper, settings, def, count: out.count, nextBumpAt: out.nextBumpAt });
  } catch (err) {
    logger.warn(`[bumps] ${guild.id}: the thank-you failed (${err?.message})`);
  }
  try {
    await rewardBumper({ guild, bumper, channel, settings });
  } catch (err) {
    logger.warn(`[bumps] ${guild.id}: the reward failed (${err?.message})`);
  }
}

// ── The reminder job ──────────────────────────────────────────────────────────

/** The first channel of `ids` the bot can post in; null when none. */
async function reminderChannel(guild, ids) {
  for (const id of ids) {
    let channel = guild.channels.cache.get(id) ?? null;
    if (!channel) {
      try {
        channel = await guild.channels.fetch(id);
      } catch (err) {
        if (PERMANENT_CODES.has(err?.code)) continue;
        throw err;
      }
    }
    if (channel && checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] }).ok) return channel;
  }
  return null;
}

/**
 * Posts the reminder for a state row. Returns false when there is nowhere to post it; throws on a Discord error a retry may fix.
 * @returns {Promise<boolean>}
 */
async function sendReminder({ guild, settings, state, def }) {
  const ids = [...new Set([settings.bumpChannelId, state.channelId].filter(Boolean))];
  const channel = await reminderChannel(guild, ids);
  if (!channel) return false;
  const text = renderPlaceholders(settings.bumpReminderMessage || DEFAULT_REMINDER_MESSAGE, {
    guild, channel, vars: { site: def.name },
  }, { maxLength: MESSAGE_MAX }).trim() || DEFAULT_REMINDER_MESSAGE;
  // The role is pinged only when the bot may ping it (mentionable, or Mention Everyone), and only that role ever is.
  const roles = settings.bumpRoleId ? pingableRoleIds(channel, guild.members.me, [settings.bumpRoleId]).allowed : [];
  const content = roles.length ? `<@&${roles[0]}> ${text}` : text;
  await channel.send({ content: content.slice(0, 2000), allowedMentions: { parse: [], roles } });
  return true;
}

/**
 * bump.remind job: when the cooldown has ended and the module is on, posts the reminder once. Idempotent: `remindedAt` is
 * claimed before the post (a retry never reminds twice) and released again when nothing was posted.
 * @param {{ guildId: string, payload: object }} job
 * @param {{ guild: object }} ctx
 */
export async function handleRemindJob(job, ctx) {
  const site = job.payload?.site;
  const def = typeof site === 'string' && Object.hasOwn(BUMP_SITES, site) ? BUMP_SITES[site] : null;
  if (!def) throw new PermanentJobError(`bump.remind: unknown site ${JSON.stringify(site)}`);
  const { guildId } = job;

  const settings = await getSettings(guildId);
  if (!isModuleEnabled(settings, 'bumps')) return undefined;
  const state = await prisma.bumpState.findUnique({ where: { guildId_site: { guildId, site } } });
  if (!state || state.remindedAt) return undefined;
  // The state moved on (a later bump): run again when it says the next bump is possible.
  if (new Date(state.nextBumpAt).getTime() - Date.now() > EARLY_SLACK_MS) return { nextRunAt: new Date(state.nextBumpAt) };

  const claimedAt = new Date();
  const claim = await prisma.bumpState.updateMany({
    where: { guildId, site, remindedAt: null, lastBumpAt: state.lastBumpAt },
    data: { remindedAt: claimedAt },
  });
  if (claim.count === 0) return undefined;
  const release = () => prisma.bumpState.updateMany({ where: { guildId, site, remindedAt: claimedAt }, data: { remindedAt: null } })
    .catch((err) => logger.warn(`[bumps] ${guildId}: could not release the reminder claim (${err?.message})`));

  let sent;
  try {
    sent = await sendReminder({ guild: ctx.guild, settings, state, def });
  } catch (err) {
    await release();
    if (PERMANENT_CODES.has(err?.code)) {
      warnLimited(guildId, `[bumps] ${guildId}: the reminder could not be posted (${err.message})`);
      return undefined;
    }
    throw err;
  }
  if (!sent) {
    await release();
    warnLimited(guildId, `[bumps] ${guildId}: no channel to post the bump reminder in (set one with /bumps setup, and give me View Channel and Send Messages there)`);
    return undefined;
  }
  logger.info(`[bumps] ${guildId}: ${site} reminder posted`);
  return undefined;
}

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

/** Sync filter for channelDelete: skipped once the memoised settings show another channel (no memo: runs). */
export function channelDeleteFilter(ctx) {
  const settings = peekSettings(ctx.guildId);
  return !settings || settings.bumpChannelId === ctx.channel?.id;
}

/** Sync filter for roleDelete, the same idea. */
export function roleDeleteFilter(ctx) {
  const settings = peekSettings(ctx.guildId);
  return !settings || settings.bumpRoleId === ctx.role?.id;
}

/** channelDelete: the reminder channel is unset (reminders then go to the channel of the last bump). */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  const guildId = ctx.guildId;
  if (!channelId || !guildId) return;
  try {
    const settings = await getSettings(guildId, { fresh: true });
    if (settings?.bumpChannelId !== channelId) return;
    await updateSettings(guildId, { bumpChannelId: null });
    logger.info(`[bumps] ${guildId}: the reminder channel was deleted, so it is unset`);
  } catch (err) {
    logger.error(`[bumps] ${guildId}: channel delete cleanup failed`, err);
  }
}

/** roleDelete: the role to ping is unset. */
export async function onRoleDelete(ctx) {
  const roleId = ctx.role?.id;
  const guildId = ctx.guildId;
  if (!roleId || !guildId) return;
  try {
    const settings = await getSettings(guildId, { fresh: true });
    if (settings?.bumpRoleId !== roleId) return;
    await updateSettings(guildId, { bumpRoleId: null });
    logger.info(`[bumps] ${guildId}: the role to ping was deleted, so it is unset`);
  } catch (err) {
    logger.error(`[bumps] ${guildId}: role delete cleanup failed`, err);
  }
}

// ── Reads and resets for the command ──────────────────────────────────────────

/** The guild's state rows, one per site that has seen a bump. */
export function getBumpStates(guildId) {
  return prisma.bumpState.findMany({ where: { guildId } });
}

/** The top bumpers: most bumps first, the earlier last bump wins a tie. */
export function topBumpers(guildId, limit = LEADERBOARD_SIZE) {
  return prisma.bumpCount.findMany({
    where: { guildId, count: { gt: 0 } },
    orderBy: [{ count: 'desc' }, { lastBumpAt: 'asc' }],
    take: limit,
  });
}

/** One member's count (0 when none). */
export async function bumpCountOf(guildId, userId) {
  const row = await prisma.bumpCount.findUnique({ where: { guildId_userId: { guildId, userId } } });
  return row?.count ?? 0;
}

/**
 * Clears bump counts: one member's, or everyone's. States and reminders are left alone.
 * @returns {Promise<number>} rows removed
 */
export async function resetCounts(guildId, userId = null) {
  const { count } = await prisma.bumpCount.deleteMany({ where: userId ? { guildId, userId } : { guildId } });
  return count;
}
