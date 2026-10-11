// Giveaways (pkg-giveaways.md): the logic behind /giveaway, the persistent Participate button, the messageDelete and
// messageDeleteBulk hooks and the `giveaway.end` scheduler job. Pure helpers (weights, the draw, packing, embeds, rows,
// references, end-time validation) come first, then the DB and Discord flows. No side effects at import: the only module
// level state is a keyed mutex and the button-count throttle map, and nothing starts a timer or does I/O until it is called.
//
// Concurrency: every state change of one giveaway runs inside `locks.run(giveawayId, …)` and every status transition is a
// conditional `updateMany` whose `where` names the expected status, so two processes that briefly overlap (a rolling
// restart) cannot both claim the same transition. A Participate click never takes the lock: it is a single atomic row
// operation guarded by @@unique([giveawayId, userId]).
import { randomInt } from 'node:crypto';
import { ActionRowBuilder, ButtonBuilder, ButtonStyle } from 'discord.js';
import prisma from '../database/client.js';
import logger from '../utils/logger.js';
import { LIMITS, COLORS } from '../config/constants.js';
import { MODULES } from '../config/modules.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { createKeyedMutex, mapLimit } from '../utils/async.js';
import { successEmbed, errorEmbed, infoEmbed, warnEmbed, guildEmbed } from '../utils/embedBuilder.js';
import { safeDefer, safeReply, checkChannelUsable, pingableRoleIds } from '../utils/interactionHelpers.js';
import { clampText, joinWithLimit, formatNumber } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';
import { scheduleJob, cancelJobs, PermanentJobError } from './scheduler.js';
import { grant, hasProvider, isRewardAvailable, providerModule } from './rewards.js';

// ── Constants (pkg-giveaways §3.4) ───────────────────────────────────────────────

export const END_JOB_TYPE = 'giveaway.end';
export const endJobKey = (giveawayId) => `giveaway.end:${giveawayId}`;
export const END_MAX_ATTEMPTS = 5;                 // ctx.attempt >= this means the final attempt
export const MIN_DURATION_MS = 60_000;             // 1 minute
export const MAX_DURATION_MS = 90 * 86_400_000;    // 90 days
export const MAX_ROLE_WEIGHTS = 10;                // per giveaway (not a per-guild cap, so not in LIMITS)
export const MIN_BONUS_WEIGHT = 2;
export const MAX_WEIGHT = 10;
export const DEFAULT_BONUS_WEIGHT = 2;
export const COUNT_REFRESH_MS = 10_000;
export const DRAW_BATCH = 100;                     // Discord's user_ids limit for a member chunk request
export const DRAW_BUDGET_MS = 30_000;              // well under the scheduler's JOB_TIMEOUT_MS (60 s)
export const MEMBER_FETCH_TIMEOUT_MS = 15_000;
export const ANNOUNCE_CHUNK = 1700;                // max characters of mentions per announcement message
export const ENTRANTS_SHOWN = 1000;
export const LIST_SHOWN = 250;
export const MAX_PRIZE_AMOUNT = 1_000_000;         // xp / coins option max
export const NOT_DUE_SLACK_MS = 60_000;            // a job that fires this much early is rescheduled

export const STATUS_TEXT = Object.freeze({ ACTIVE: 'still running', DRAFT: 'a draft', ENDED: 'already ended', CANCELLED: 'cancelled' });
export const STATUS_TEXT_SHORT = Object.freeze({ ACTIVE: 'running', DRAFT: 'draft', ENDED: 'ended', CANCELLED: 'cancelled' });
export const STATUS_EMOJI = Object.freeze({ ACTIVE: '🟢', DRAFT: '📝', ENDED: '🏁', CANCELLED: '🚫' });

/** Permissions the bot needs in the giveaway channel to post, edit and reply. */
export const POST_NEEDS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory']);

const OPEN_STATUSES = ['ACTIVE', 'DRAFT'];
const locks = createKeyedMutex();

// ── Pure helpers ─────────────────────────────────────────────────────────────────

const SNOWFLAKE_RE = /^\d{17,20}$/;

/**
 * Pure, never throws. Keeps `{ roleId, weight }` entries with a snowflake roleId and an integer weight 1..10,
 * de-duplicates by roleId (highest weight wins), sorts by weight descending then roleId, and keeps the first 10.
 * @param {unknown} raw
 * @returns {Array<{ roleId: string, weight: number }>}
 */
export function normalizeRoleWeights(raw) {
  if (!Array.isArray(raw)) return [];
  const best = new Map();
  for (const entry of raw) {
    if (!entry || typeof entry !== 'object') continue;
    const { roleId, weight } = entry;
    if (typeof roleId !== 'string' || !SNOWFLAKE_RE.test(roleId)) continue;
    if (!Number.isInteger(weight) || weight < 1 || weight > MAX_WEIGHT) continue;
    if (!best.has(roleId) || best.get(roleId) < weight) best.set(roleId, weight);
  }
  return [...best].map(([roleId, weight]) => ({ roleId, weight }))
    .sort((a, b) => b.weight - a.weight || (a.roleId < b.roleId ? -1 : a.roleId > b.roleId ? 1 : 0))
    .slice(0, MAX_ROLE_WEIGHTS);
}

/** The highest matching weight wins; 1 is the default. Pure. */
export function memberWeight(hasRole, weights) {
  let w = 1;
  for (const entry of weights ?? []) if (entry.weight > w && hasRole(entry.roleId)) w = entry.weight;
  return w;
}

/** A member must hold every required role. Pure. */
export function meetsRequirements(hasRole, requiredRoleIds) {
  return (requiredRoleIds ?? []).every((id) => hasRole(id));
}

/** The first denied role the member holds, or null. Pure. */
export function heldDeniedRole(hasRole, deniedRoleIds) {
  for (const id of deniedRoleIds ?? []) if (hasRole(id)) return id;
  return null;
}

/** @returns {number} a uniform random number in (0, 1) from the OS CSPRNG */
export function secureRandom() {
  return randomInt(1, 2 ** 48) / 2 ** 48;
}

/**
 * Exact weighted sampling without replacement (Efraimidis-Spirakis A-ES), evaluated lazily: candidates are visited in
 * descending order of an upper bound of their key, so the member lookups stop as soon as no unvisited entrant can still
 * enter the top `count`. See pkg-giveaways §6.12 for the proof.
 * @param {object} opts
 * @param {string[]} opts.userIds
 * @param {number} opts.count
 * @param {number} [opts.maxWeight=1] the largest weight any member can have
 * @param {(ids: string[]) => Promise<Map<string, { weight: number }|null>>} opts.resolve null = ineligible, missing = unknown
 * @param {() => number} [opts.rng]
 * @param {number} [opts.batchSize]
 * @param {number} [opts.deadline] epoch ms
 * @param {() => number} [opts.now]
 * @returns {Promise<{ winnerIds: string[], complete: boolean, unknown: number }>}
 */
export async function drawWinners({
  userIds, count, maxWeight = 1, resolve, rng = secureRandom, batchSize = DRAW_BATCH, deadline = Infinity, now = Date.now,
}) {
  const ids = [...new Set(userIds ?? [])];
  if (!(count > 0) || ids.length === 0) return { winnerIds: [], complete: true, unknown: 0 };
  const W = Math.max(1, maxWeight);
  // One rng() call per id, in input order.
  const items = ids.map((id) => {
    const u = rng();
    return { id, u, ub: u ** (1 / W) };
  });
  items.sort((a, b) => b.ub - a.ub);

  const best = []; // { id, key }, sorted by key descending, at most `count` long
  let unknown = 0;
  let i = 0;
  while (i < items.length) {
    if (best.length === count && best[count - 1].key >= items[i].ub) break; // nothing left can enter the top `count`
    if (now() > deadline) return { winnerIds: best.map((b) => b.id), complete: false, unknown };
    const batch = items.slice(i, i + batchSize);
    i += batch.length;
    const res = await resolve(batch.map((b) => b.id));
    for (const b of batch) {
      if (!res.has(b.id)) { unknown++; continue; }
      const r = res.get(b.id);
      if (r === null) continue;
      const key = b.u ** (1 / r.weight);
      let at = best.length;
      while (at > 0 && best[at - 1].key < key) at--;
      if (at < count) {
        best.splice(at, 0, { id: b.id, key });
        if (best.length > count) best.pop();
      }
    }
  }
  return { winnerIds: best.map((b) => b.id), complete: true, unknown };
}

const activeWeights = (g, guild) => normalizeRoleWeights(g.roleWeights).filter((w) => guild.roles.cache.has(w.roleId));

/** The largest weight a member of this giveaway can have; deleted bonus roles are ignored. */
export function maxWeightFor(g, guild) {
  return Math.max(1, ...activeWeights(g, guild).map((w) => w.weight));
}

/**
 * Builds the member resolver for drawWinners: members from the cache, the rest through ONE gateway chunk request per
 * batch (at most 100 ids). A deleted required or denied role no longer restricts anyone. An id missing from a successful
 * response has left the server (null); a failed request leaves the ids unset, which counts as "unknown".
 */
export function makeResolver(guild, g) {
  const required = (g.requiredRoleIds ?? []).filter((id) => guild.roles.cache.has(id));
  const denied = (g.deniedRoleIds ?? []).filter((id) => guild.roles.cache.has(id));
  const weights = activeWeights(g, guild);
  const evaluate = (m) => {
    if (m.user?.bot) return null;
    const has = (r) => m.roles.cache.has(r);
    if (!meetsRequirements(has, required)) return null;
    if (heldDeniedRole(has, denied)) return null;
    return { weight: memberWeight(has, weights) };
  };
  return async (ids) => {
    const out = new Map();
    const misses = [];
    for (const id of ids) {
      const m = guild.members.cache.get(id);
      if (m && !m.partial) out.set(id, evaluate(m));
      else misses.push(id);
    }
    if (misses.length) {
      try {
        const fetched = await guild.members.fetch({ user: misses, time: MEMBER_FETCH_TIMEOUT_MS });
        for (const id of misses) out.set(id, fetched.has(id) ? evaluate(fetched.get(id)) : null);
      } catch (err) {
        logger.warn(`Giveaway member lookup failed for ${misses.length} entrant(s) in guild ${guild.id}`, err);
      }
    }
    return out;
  };
}

/** `<@id>` mentions joined with ', ', packed greedily into chunks of at most `maxLen` characters; a mention is never split. */
export function packMentions(userIds, maxLen = ANNOUNCE_CHUNK) {
  const chunks = [];
  let cur = '';
  for (const id of userIds ?? []) {
    const m = `<@${id}>`;
    if (cur === '') cur = m;
    else if (cur.length + 2 + m.length <= maxLen) cur += `, ${m}`;
    else { chunks.push(cur); cur = m; }
  }
  if (cur !== '') chunks.push(cur);
  return chunks;
}

const idsIn = (chunk) => [...String(chunk).matchAll(/<@(\d+)>/g)].map((m) => m[1]);

/**
 * @param {unknown} input an id, a message id or a message link
 * @returns {{ kind: 'id'|'message', value: string, guildId?: string }|null}
 */
export function parseGiveawayRef(input) {
  const s = String(input ?? '').trim();
  const link = /^https?:\/\/(?:(?:ptb|canary)\.)?discord(?:app)?\.com\/channels\/(\d{17,20})\/(\d{17,20})\/(\d{17,20})\/?$/.exec(s);
  if (link) return { kind: 'message', value: link[3], guildId: link[1] };
  if (SNOWFLAKE_RE.test(s)) return { kind: 'message', value: s };
  if (/^c[a-z0-9]{20,31}$/.test(s)) return { kind: 'id', value: s };
  return null;
}

/** Looks a giveaway up in this guild by id or message id/link. @returns {Promise<object|null>} */
export async function resolveGiveawayRef(guildId, input) {
  const ref = parseGiveawayRef(input);
  if (!ref) return null;
  if (ref.guildId && ref.guildId !== guildId) return null;
  if (ref.kind === 'id') return prisma.giveaway.findFirst({ where: { id: ref.value, guildId } });
  return prisma.giveaway.findFirst({ where: { messageId: ref.value, guildId } });
}

/**
 * Pure. Checks the end time of a giveaway.
 * @param {unknown} endsAt
 * @param {Date} now
 * @param {string} [tz]
 * @returns {{ ok: true }|{ ok: false, title: string, message: string }}
 */
export function validateEndsAt(endsAt, now, tz = 'UTC') {
  if (!(endsAt instanceof Date) || Number.isNaN(endsAt.getTime())) {
    return {
      ok: false,
      title: 'Invalid Duration',
      message: `I couldn't understand that. Use a length like \`30m\`, \`2h\` or \`3d\`, or an end time like \`2026-12-24 18:00\` (server timezone: ${tz}).`,
    };
  }
  const diff = endsAt.getTime() - now.getTime();
  if (diff <= 0) return { ok: false, title: 'Invalid Duration', message: `That end time is in the past (server timezone: ${tz}).` };
  if (diff < MIN_DURATION_MS) return { ok: false, title: 'Too Short', message: 'A giveaway must run for at least 1 minute.' };
  if (diff > MAX_DURATION_MS) return { ok: false, title: 'Too Long', message: 'A giveaway can run for at most 90 days.' };
  return { ok: true };
}

/**
 * Pure. Why a prize of this kind cannot be paid right now, or null when it can.
 * @param {'xp'|'coins'} kind
 * @param {object|null} settings
 * @returns {string|null}
 */
export function rewardUnavailableMessage(kind, settings) {
  if (isRewardAvailable(kind, settings)) return null;
  const key = providerModule(kind);
  const m = MODULES[key];
  const label = kind === 'xp' ? 'XP' : 'Coin';
  if (!hasProvider(kind)) return `${label} prizes are not available on this bot (the ${m.emoji} ${m.label} module is not installed).`;
  return `${label} prizes need the **${m.emoji} ${m.label}** module. Enable it with \`/module enable ${key}\`.`;
}

/** One line of `/giveaway list`. */
export function formatListLine(row) {
  const n = row._count?.entries ?? 0;
  const ends = row.endsAt;
  let when;
  if (row.status === 'ACTIVE') when = `ends ${discordTimestamp(ends, 'R')}`;
  else if (row.status === 'DRAFT') when = 'draft';
  else if (row.status === 'ENDED') when = `ended ${discordTimestamp(row.endedAt ?? ends, 'R')}`;
  else when = 'cancelled';
  return `${STATUS_EMOJI[row.status] ?? '🎉'} **${clampText(row.prize, 80)}** · <#${row.channelId}> · ${when} · ${formatNumber(n)} ${n === 1 ? 'entry' : 'entries'}\n\`${row.id}\``;
}

// ── Embeds and rows (pure) ────────────────────────────────────────────────────────

/** The Participate button row. The count is refreshed by scheduleCountRefresh. */
export function buildJoinRow(giveawayId, count, { disabled = false } = {}) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder()
      .setCustomId(buildCustomId('giveaway', 'join', giveawayId))
      .setStyle(ButtonStyle.Primary)
      .setEmoji('🎉')
      .setLabel(`Participate (${formatNumber(count)})`)
      .setDisabled(disabled),
  );
}

function prizeParts(g, settings) {
  const parts = [];
  if (g.xpPrize > 0) parts.push(`\`${formatNumber(g.xpPrize)} XP\``);
  if (g.coinPrize > 0) parts.push(`\`${settings?.currencyEmoji ?? '🪙'} ${formatNumber(g.coinPrize)} ${settings?.currencyName ?? 'coins'}\``);
  return parts;
}

const roleList = (ids, roleExists) => ids.filter((id) => roleExists(id)).map((id) => `<@&${id}>`).join(', ');

/**
 * The giveaway message embed. ACTIVE (and DRAFT, as a preview), ENDED and CANCELLED look different. Prize and description
 * are user text, rendered literally (MEE6 giveaways have no variables) and clamped.
 * @param {object} g a Giveaway row (or the fields of one)
 * @param {{ settings?: object|null, entryCount?: number, roleExists?: (id: string) => boolean, now?: Date }} [opts]
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildGiveawayEmbed(g, { settings = null, entryCount = 0, roleExists = () => true, now = new Date() } = {}) {
  const block = g.description ? [clampText(g.description, 1000), ''] : [];
  const hosted = `**Hosted by:** <@${g.hostId}>`;
  const prize = prizeParts(g, settings);

  if (g.status === 'CANCELLED') {
    const lines = [...block, 'This giveaway was cancelled.', hosted];
    return guildEmbed(settings, clampText(`🚫 ${g.prize}`, 256), lines.join('\n'), `ID: ${g.id} · Cancelled`)
      .setColor(COLORS.NEUTRAL)
      .setTimestamp(g.endedAt ?? now);
  }

  if (g.status === 'ENDED') {
    const winners = g.winnerIds?.length
      ? joinWithLimit(g.winnerIds.map((id) => `<@${id}>`), { max: 1024 })
      : 'No valid entrants.';
    const lines = [
      ...block,
      `**Ended:** ${discordTimestamp(g.endedAt ?? now, 'R')}`,
      hosted,
      `**Winners:** ${winners}`,
      `**Entries:** ${formatNumber(entryCount)}`,
    ];
    if (prize.length) lines.push(`**Bonus prize:** ${prize.join(' + ')} each`);
    return guildEmbed(settings, clampText(`🎉 ${g.prize}`, 256), lines.join('\n'), `ID: ${g.id} · Ended at`)
      .setColor(COLORS.NEUTRAL)
      .setTimestamp(g.endedAt ?? now);
  }

  // ACTIVE and DRAFT
  const lines = [
    ...block,
    'Click **🎉 Participate** below to enter. Click it again to leave.',
    '',
    `**Ends:** ${discordTimestamp(g.endsAt, 'R')} (${discordTimestamp(g.endsAt, 'f')})`,
    hosted,
    `**Winners:** ${g.winnerCount}`,
  ];
  const required = roleList(g.requiredRoleIds ?? [], roleExists);
  if (required) lines.push(`**Required role:** ${required}`);
  const denied = roleList(g.deniedRoleIds ?? [], roleExists);
  if (denied) lines.push(`**Not for:** ${denied}`);
  const weights = normalizeRoleWeights(g.roleWeights).filter((w) => roleExists(w.roleId));
  if (weights.length) lines.push(`**Bonus odds:** ${weights.map((w) => `<@&${w.roleId}> ×${w.weight}`).join(', ')}`);
  if (prize.length) lines.push(`**Bonus prize:** ${prize.join(' + ')} each`);
  const embed = guildEmbed(settings, clampText(`🎉 ${g.prize}`, 256), clampText(lines.join('\n'), 4096), `ID: ${g.id} · Ends at`)
    .setTimestamp(g.endsAt);
  if (Number.isInteger(g.embedColor) && g.embedColor >= 0 && g.embedColor <= 0xffffff) embed.setColor(g.embedColor);
  return embed;
}

// ── Discord lookups ───────────────────────────────────────────────────────────────

const NO_ACCESS_CODES = new Set([50001, 50013]);

/**
 * The channel of a giveaway: cache first, then REST. Null only on 10003 (Unknown Channel); any other error is rethrown
 * (00-overview rule 20: a cache miss is never a deletion).
 */
export async function fetchChannel(guild, channelId) {
  const cached = guild.channels.cache.get(channelId);
  if (cached) return cached;
  try {
    return (await guild.channels.fetch(channelId)) ?? null;
  } catch (err) {
    if (err?.code === 10003) return null;
    throw err;
  }
}

/**
 * Finds the channel and the message of a giveaway.
 * @returns {Promise<{ channel: object, message: object }
 *   | { gone: true, reason: string, channel?: object }
 *   | { noAccess: true, missing: string[], reason?: string, channel?: object }>}
 */
export async function locateMessage(guild, g) {
  let channel;
  try {
    channel = await fetchChannel(guild, g.channelId);
  } catch (err) {
    if (NO_ACCESS_CODES.has(err?.code)) return { noAccess: true, missing: ['ViewChannel'] };
    throw err;
  }
  if (!channel) return { gone: true, reason: 'channel deleted' };
  const usable = checkChannelUsable(channel, { needs: [...POST_NEEDS] });
  if (!usable.ok) return { noAccess: true, missing: usable.missing, reason: usable.reason, channel };
  if (!g.messageId) return { gone: true, reason: 'message missing', channel };
  try {
    const message = await channel.messages.fetch(g.messageId);
    return { channel, message };
  } catch (err) {
    if (err?.code === 10008) return { gone: true, reason: 'message deleted', channel };
    if (NO_ACCESS_CODES.has(err?.code)) return { noAccess: true, missing: ['ViewChannel', 'ReadMessageHistory'], channel };
    throw err;
  }
}

const roleExistsIn = (guild) => (id) => guild.roles.cache.has(id);

/** Edits the embed and the button of a giveaway message. Never throws; a 10008 is ignored. */
async function editGiveawayMessage(channel, messageId, g, { settings, entryCount, disabled }) {
  try {
    await channel.messages.edit(messageId, {
      embeds: [buildGiveawayEmbed(g, { settings, entryCount, roleExists: roleExistsIn(channel.guild) })],
      components: [buildJoinRow(g.id, entryCount, { disabled })],
    });
  } catch (err) {
    if (err?.code === 10008) return;
    logger.warn(`Giveaway ${g.id}: could not edit the message`, err);
  }
}

// ── Prizes ────────────────────────────────────────────────────────────────────────

const REASON_TEXT = (reason, m) => {
  if (reason === 'unavailable') return `the ${m.emoji} ${m.label} module is not installed`;
  if (reason === 'module-disabled') return `the ${m.emoji} ${m.label} module is turned off`;
  if (reason === 'invalid-amount') return 'the amount is invalid';
  return 'an error occurred';
};

/**
 * Pays the XP and coin prizes through the rewards bus (the only way giveaways touch XP or coins). Never throws.
 * @returns {Promise<{ warnings: string[], results: Record<string, { ok: number, failed: number, reasons: string[] }> }>}
 */
export async function payPrizes({ guild, giveaway, winnerIds, channel = null }) {
  const kinds = [];
  if (giveaway.xpPrize > 0) kinds.push(['xp', giveaway.xpPrize]);
  if (giveaway.coinPrize > 0) kinds.push(['coins', giveaway.coinPrize]);
  const results = {};
  for (const [kind] of kinds) results[kind] = { ok: 0, failed: 0, reasons: [] };
  const warnings = [];
  if (!kinds.length || !winnerIds?.length) return { warnings, results };
  try {
    await mapLimit(winnerIds, 3, async (userId) => {
      for (const [kind, amount] of kinds) {
        let r;
        try {
          r = await grant(kind, {
            guild, userId, amount,
            reason: `Giveaway prize: ${clampText(giveaway.prize, 80)}`,
            member: guild.members.cache.get(userId) ?? null,
            channel,
          });
        } catch (err) {
          r = { ok: false, reason: 'error' };
          logger.error(`Giveaway ${giveaway.id}: ${kind} grant threw`, err);
        }
        if (r?.ok) results[kind].ok++;
        else { results[kind].failed++; results[kind].reasons.push(r?.reason ?? 'error'); }
      }
    });
    for (const [kind] of kinds) {
      const t = results[kind];
      if (!t.failed) continue;
      const m = MODULES[providerModule(kind)];
      warnings.push(
        `⚠️ The ${kind === 'xp' ? 'XP' : 'coin'} prize could not be paid to ${t.failed} winner${t.failed === 1 ? '' : 's'}: ${REASON_TEXT(t.reasons[0], m)}. Please pay it manually.`,
      );
    }
  } catch (err) {
    logger.error(`Giveaway ${giveaway.id}: paying prizes failed`, err);
    warnings.push('⚠️ The prizes could not be paid. Please pay them manually.');
  }
  return { warnings, results };
}

function prizeLine(g, settings) {
  const parts = [];
  if (g.xpPrize > 0) parts.push(`**${formatNumber(g.xpPrize)} XP**`);
  if (g.coinPrize > 0) parts.push(`**${settings?.currencyEmoji ?? '🪙'} ${formatNumber(g.coinPrize)} ${settings?.currencyName ?? 'coins'}**`);
  return parts.length ? `Each winner also gets ${parts.join(' and ')}.` : null;
}

/**
 * Announces winners in the giveaway channel. Pings the winners only. Never throws.
 * @returns {Promise<boolean>} true when the first message was sent
 */
export async function announceWinners(channel, g, newWinnerIds, { settings = null, reroll = false } = {}) {
  try {
    const reply = g.messageId ? { messageReference: g.messageId, failIfNotExists: false } : undefined;
    if (!newWinnerIds.length) {
      await channel.send({
        content: `No valid entrants for **${clampText(g.prize, 200)}**, so no winner could be chosen.`,
        reply,
        allowedMentions: { parse: [] },
      });
      return true;
    }
    const chunks = packMentions(newWinnerIds, ANNOUNCE_CHUNK);
    const line = prizeLine(g, settings);
    const n = newWinnerIds.length;
    const head = reroll
      ? `🎉 New winner${n === 1 ? '' : 's'}: ${chunks[0]}! Congratulations, you won **${clampText(g.prize, 200)}**!`
      : `🎉 Congratulations ${chunks[0]}! You won **${clampText(g.prize, 200)}**!`;
    await channel.send({
      content: line ? `${head}\n${line}` : head,
      reply,
      allowedMentions: { parse: [], users: idsIn(chunks[0]) },
    });
    for (const chunk of chunks.slice(1)) {
      try {
        await channel.send({ content: `🎉 ${chunk}`, allowedMentions: { parse: [], users: idsIn(chunk) } });
      } catch (err) {
        logger.warn(`Giveaway ${g.id}: could not send an announcement chunk`, err);
      }
    }
    return true;
  } catch (err) {
    logger.warn(`Giveaway ${g.id}: could not announce the winners`, err);
    return false;
  }
}

async function sendWarnings(channel, warnings) {
  if (!channel || !warnings.length) return;
  try {
    await channel.send({ content: clampText(warnings.join('\n'), 2000), allowedMentions: { parse: [] } });
  } catch (err) {
    logger.warn('Giveaway: could not post the prize warnings', err);
  }
}

// ── Flows ─────────────────────────────────────────────────────────────────────────

const failure = (code, extra = {}) => ({ ok: false, code, ...extra });

/** The send-time role ping: the executor is the bot (00-overview rule 23). */
function pingFor(channel, guild, pingRoleId) {
  if (!pingRoleId) return [];
  return pingableRoleIds(channel, guild.members?.me ?? null, [pingRoleId]).allowed;
}

async function postGiveawayMessage(channel, guild, g, { settings, entryCount, pingRoleId }) {
  const ping = pingFor(channel, guild, pingRoleId);
  return channel.send({
    content: ping.length ? `<@&${ping[0]}>` : undefined,
    embeds: [buildGiveawayEmbed(g, { settings, entryCount, roleExists: roleExistsIn(guild) })],
    components: [buildJoinRow(g.id, entryCount)],
    allowedMentions: { parse: [], roles: ping },
  });
}

const scheduleEnd = (guildId, giveawayId, runAt) => scheduleJob({
  guildId, type: END_JOB_TYPE, runAt, payload: { giveawayId }, dedupeKey: endJobKey(giveawayId),
  maxAttempts: END_MAX_ATTEMPTS, onConflict: 'replace',
});

/**
 * Creates a giveaway, posts its message and schedules the end job.
 * @returns {Promise<{ ok: true, giveaway: object, message: object }|{ ok: false, code: 'LIMIT'|'SEND_FAILED'|'DB_ERROR', error?: unknown }>}
 */
export async function startGiveaway(opts) {
  const {
    guild, channel, hostId, prize, description = null, winnerCount, endsAt, requiredRoleIds = [], deniedRoleIds = [],
    roleWeights = [], xpPrize = null, coinPrize = null, pingRoleId = null, embedColor = null, settings = null,
  } = opts;
  return locks.run(`start:${guild.id}`, async () => {
    let g;
    try {
      const open = await prisma.giveaway.count({ where: { guildId: guild.id, status: { in: OPEN_STATUSES } } });
      if (open >= LIMITS.ACTIVE_GIVEAWAYS) return failure('LIMIT');
      await ensureGuildSettings(guild.id);
      g = await prisma.giveaway.create({
        data: {
          guildId: guild.id, channelId: channel.id, hostId, prize, description, winnerCount, requiredRoleIds, deniedRoleIds,
          roleWeights: normalizeRoleWeights(roleWeights), xpPrize: xpPrize ?? null, coinPrize: coinPrize ?? null,
          pingRoleId, embedColor, endsAt, status: 'ACTIVE',
        },
      });
    } catch (err) {
      logger.error('Giveaway create failed', err);
      return failure('DB_ERROR', { error: err });
    }

    let message;
    try {
      message = await postGiveawayMessage(channel, guild, g, { settings, entryCount: 0, pingRoleId });
    } catch (err) {
      logger.warn(`Giveaway ${g.id}: could not post in ${channel.id} (Discord error ${err?.code ?? 'unknown'})`, err);
      await prisma.giveaway.delete({ where: { id: g.id } }).catch(() => {});
      return failure('SEND_FAILED', { error: err });
    }

    try {
      await prisma.giveaway.update({ where: { id: g.id }, data: { messageId: message.id } });
      await scheduleEnd(guild.id, g.id, endsAt);
    } catch (err) {
      logger.error(`Giveaway ${g.id}: could not record or schedule the giveaway`, err);
      // Delete the row first so the messageDelete hook finds nothing, then the message.
      await prisma.giveaway.delete({ where: { id: g.id } }).catch(() => {});
      await message.delete().catch(() => {});
      return failure('DB_ERROR', { error: err });
    }
    return { ok: true, giveaway: { ...g, messageId: message.id }, message };
  });
}

/**
 * Ends a giveaway: draws, claims, edits the message, announces and pays. Throws (so the scheduler retries) only for a job
 * that is not on its final attempt and hit a transient problem, or on unexpected DB/Discord errors.
 * @param {{ client: object, guild: object, giveawayId: string, source: 'job'|'command', attempt?: number }} opts
 * @returns {Promise<object>} { ok: true, winnerIds, entryCount, announced, payWarnings } or { ok: false, code, … }
 */
export function endGiveaway({ client, guild, giveawayId, source, attempt = 1 }) {
  const final = source === 'job' && attempt >= END_MAX_ATTEMPTS;
  const retry = source === 'job' && !final;
  return locks.run(giveawayId, async () => {
    const g = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
    if (!g || g.guildId !== guild.id) return failure('NOT_FOUND');
    if (g.status !== 'ACTIVE') return failure('NOT_ACTIVE');
    // Defensive: replace-scheduling normally prevents an early run.
    if (source === 'job' && g.endsAt.getTime() - Date.now() > NOT_DUE_SLACK_MS) return failure('NOT_DUE', { nextRunAt: g.endsAt });
    if (guild.available === false) {
      // Rule 20: nothing is concluded about a guild whose caches are empty.
      if (retry) throw new Error(`giveaway ${g.id}: guild ${guild.id} is unavailable`);
      return failure('RESOLVE_FAILED');
    }

    const settings = await getSettingsSafe(guild.id);
    const loc = await locateMessage(guild, g);
    let canPost = false;
    let channel = null;
    if (loc.gone) {
      const moved = await prisma.giveaway.updateMany({ where: { id: g.id, status: 'ACTIVE' }, data: { status: 'DRAFT', messageId: null } });
      if (!moved.count) return failure('NOT_ACTIVE');
      await cancelJobs({ dedupeKey: endJobKey(g.id) });
      logger.info(`Giveaway ${g.id} moved to draft: ${loc.reason}`);
      return failure('DRAFTED');
    }
    if (loc.noAccess) {
      if (source === 'command') return failure('NO_ACCESS', { missing: loc.missing, reason: loc.reason });
      if (retry) throw new Error(`giveaway ${g.id}: channel unusable (${loc.missing.join(',') || loc.reason || 'unknown'})`);
      // final attempt: end without posting
    } else {
      canPost = true;
      channel = loc.channel;
    }

    const entries = await prisma.giveawayEntry.findMany({ where: { giveawayId: g.id }, select: { userId: true }, orderBy: { createdAt: 'asc' } });
    const entryCount = entries.length;
    const draw = await drawWinners({
      userIds: entries.map((e) => e.userId),
      count: g.winnerCount,
      maxWeight: maxWeightFor(g, guild),
      resolve: makeResolver(guild, g),
      deadline: Date.now() + DRAW_BUDGET_MS,
    });
    if (!draw.complete || draw.unknown > 0) {
      if (source === 'command') return failure('RESOLVE_FAILED');
      if (retry) throw new Error(`giveaway ${g.id}: could not check every entrant (${draw.unknown} unknown, complete=${draw.complete})`);
      logger.warn(`Giveaway ${g.id}: ending with ${draw.unknown} unresolved entrant(s) (complete=${draw.complete})`);
    }

    // The claim is the idempotency gate: prizes are paid only after it, so they are paid at most once.
    const endedAt = new Date();
    const claim = await prisma.giveaway.updateMany({
      where: { id: g.id, status: 'ACTIVE' },
      data: { status: 'ENDED', endedAt, winnerIds: draw.winnerIds },
    });
    if (!claim.count) return failure('NOT_ACTIVE');
    await cancelJobs({ dedupeKey: endJobKey(g.id) });

    // From here on nothing may throw.
    const ended = { ...g, status: 'ENDED', endedAt, winnerIds: draw.winnerIds };
    let announced = false;
    let payWarnings = [];
    try {
      if (canPost) {
        await editGiveawayMessage(channel, g.messageId, ended, { settings, entryCount, disabled: true });
        announced = await announceWinners(channel, ended, draw.winnerIds, { settings, reroll: false });
      }
      const pay = await payPrizes({ guild, giveaway: ended, winnerIds: draw.winnerIds, channel: canPost ? channel : null });
      payWarnings = pay.warnings;
      for (const w of payWarnings) logger.warn(`Giveaway ${g.id}: ${w}`);
      if (canPost) await sendWarnings(channel, payWarnings);
    } catch (err) {
      logger.error(`Giveaway ${g.id}: finishing the end failed`, err);
    }
    return { ok: true, winnerIds: draw.winnerIds, entryCount, announced, payWarnings };
  });
}

/**
 * Draws `count` more winners of an ended giveaway, excluding everyone drawn before.
 * @returns {Promise<{ ok: true, newWinnerIds: string[], announced: boolean, payWarnings: string[] }
 *   |{ ok: false, code: 'NOT_FOUND'|'NOT_ENDED'|'NO_ENTRANTS'|'CONFLICT'|'RESOLVE_FAILED' }>}
 */
export function rerollGiveaway({ guild, giveawayId, count }) {
  return locks.run(giveawayId, async () => {
    const g = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
    if (!g || g.guildId !== guild.id) return failure('NOT_FOUND');
    if (g.status !== 'ENDED') return failure('NOT_ENDED');

    const entries = await prisma.giveawayEntry.findMany({ where: { giveawayId: g.id }, select: { userId: true }, orderBy: { createdAt: 'asc' } });
    const exclude = new Set(g.winnerIds);
    const draw = await drawWinners({
      userIds: entries.map((e) => e.userId).filter((id) => !exclude.has(id)),
      count,
      maxWeight: maxWeightFor(g, guild),
      resolve: makeResolver(guild, g),
      deadline: Date.now() + DRAW_BUDGET_MS,
    });
    if (!draw.complete || draw.unknown > 0) return failure('RESOLVE_FAILED');
    if (!draw.winnerIds.length) return failure('NO_ENTRANTS');

    const winnerIds = [...g.winnerIds, ...draw.winnerIds];
    const claim = await prisma.giveaway.updateMany({ where: { id: g.id, status: 'ENDED', updatedAt: g.updatedAt }, data: { winnerIds } });
    if (!claim.count) return failure('CONFLICT');

    const updated = { ...g, winnerIds };
    let announced = false;
    let payWarnings = [];
    try {
      const settings = await getSettingsSafe(guild.id);
      let loc = null;
      try {
        loc = await locateMessage(guild, g);
      } catch (err) {
        logger.warn(`Giveaway ${g.id}: could not locate the message for a reroll`, err);
      }
      const channel = loc?.channel && checkChannelUsable(loc.channel, { needs: [...POST_NEEDS] }).ok ? loc.channel : null;
      if (channel && loc.message) {
        await editGiveawayMessage(channel, g.messageId, updated, { settings, entryCount: entries.length, disabled: true });
      }
      if (channel) announced = await announceWinners(channel, updated, draw.winnerIds, { settings, reroll: true });
      const pay = await payPrizes({ guild, giveaway: updated, winnerIds: draw.winnerIds, channel });
      payWarnings = pay.warnings;
      for (const w of payWarnings) logger.warn(`Giveaway ${g.id}: ${w}`);
      await sendWarnings(channel, payWarnings);
    } catch (err) {
      logger.error(`Giveaway ${g.id}: finishing the reroll failed`, err);
    }
    return { ok: true, newWinnerIds: draw.winnerIds, announced, payWarnings };
  });
}

/**
 * Cancels an ACTIVE or DRAFT giveaway without drawing. Entries are kept.
 * @returns {Promise<{ ok: true }|{ ok: false, code: 'NOT_OPEN'|'NOT_FOUND' }>}
 */
export function cancelGiveaway({ guild, giveawayId }) {
  return locks.run(giveawayId, async () => {
    const g = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
    if (!g || g.guildId !== guild.id) return failure('NOT_FOUND');
    const endedAt = new Date();
    const moved = await prisma.giveaway.updateMany({
      where: { id: g.id, status: { in: OPEN_STATUSES } },
      data: { status: 'CANCELLED', endedAt },
    });
    if (!moved.count) return failure('NOT_OPEN');
    await cancelJobs({ dedupeKey: endJobKey(g.id) });
    if (g.messageId) {
      try {
        const loc = await locateMessage(guild, g);
        if (loc.channel && loc.message) {
          const settings = await getSettingsSafe(guild.id);
          const entryCount = await prisma.giveawayEntry.count({ where: { giveawayId: g.id } });
          await editGiveawayMessage(loc.channel, g.messageId, { ...g, status: 'CANCELLED', endedAt }, { settings, entryCount, disabled: true });
        }
      } catch (err) {
        logger.warn(`Giveaway ${g.id}: could not update the message after cancelling`, err);
      }
    }
    return { ok: true };
  });
}

/**
 * Republishes a draft giveaway (its message was deleted) with a fresh message and end job. Entries are kept.
 * @returns {Promise<{ ok: true, message: object }|{ ok: false, code: 'NOT_DRAFT'|'CONFLICT'|'SEND_FAILED'|'DB_ERROR', error?: unknown }>}
 */
export function restartGiveaway({ guild, giveawayId, channel, endsAt, settings = null }) {
  return locks.run(giveawayId, async () => {
    let g;
    let entryCount;
    try {
      g = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
      if (!g || g.guildId !== guild.id || g.status !== 'DRAFT') return failure('NOT_DRAFT');
      entryCount = await prisma.giveawayEntry.count({ where: { giveawayId } });
    } catch (err) {
      logger.error(`Giveaway ${giveawayId}: restart lookup failed`, err);
      return failure('DB_ERROR', { error: err });
    }
    const pingRoleId = g.pingRoleId && guild.roles.cache.has(g.pingRoleId) ? g.pingRoleId : null;

    let message;
    try {
      message = await postGiveawayMessage(channel, guild, { ...g, status: 'ACTIVE', endsAt, channelId: channel.id }, { settings, entryCount, pingRoleId });
    } catch (err) {
      logger.warn(`Giveaway ${g.id}: could not repost in ${channel.id} (Discord error ${err?.code ?? 'unknown'})`, err);
      return failure('SEND_FAILED', { error: err });
    }

    try {
      const moved = await prisma.giveaway.updateMany({
        where: { id: g.id, status: 'DRAFT' },
        data: { status: 'ACTIVE', channelId: channel.id, messageId: message.id, endsAt },
      });
      if (!moved.count) {
        await message.delete().catch(() => {});
        return failure('CONFLICT');
      }
    } catch (err) {
      logger.error(`Giveaway ${g.id}: restart transition failed`, err);
      await message.delete().catch(() => {});
      return failure('DB_ERROR', { error: err });
    }

    try {
      await scheduleEnd(guild.id, g.id, endsAt);
    } catch (err) {
      logger.error(`Giveaway ${g.id}: could not schedule the end after a restart`, err);
      // Clear messageId before deleting the message so the messageDelete hook has nothing left to do.
      await prisma.giveaway.updateMany({ where: { id: g.id, status: 'ACTIVE', messageId: message.id }, data: { status: 'DRAFT', messageId: null } }).catch(() => {});
      await message.delete().catch(() => {});
      return failure('DB_ERROR', { error: err });
    }
    return { ok: true, message };
  });
}

/**
 * Adds, changes or removes a role's bonus odds on an ACTIVE or DRAFT giveaway, and refreshes the embed.
 * @param {{ guild: object, giveawayId: string, op: 'add'|'remove', roleId: string, weight?: number, settings?: object|null }} opts
 * @returns {Promise<{ ok: true, roleWeights: object[] }|{ ok: false, code: 'NOT_FOUND'|'NOT_OPEN'|'LIMIT'|'NOT_SET' }>}
 */
export function updateOdds({ guild, giveawayId, op, roleId, weight, settings = null }) {
  return locks.run(giveawayId, async () => {
    const g = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
    if (!g || g.guildId !== guild.id) return failure('NOT_FOUND');
    if (!OPEN_STATUSES.includes(g.status)) return failure('NOT_OPEN');

    const current = normalizeRoleWeights(g.roleWeights);
    const others = current.filter((w) => w.roleId !== roleId);
    let next;
    if (op === 'add') {
      // Checked before normalizing, which would silently drop an 11th role. Replacing a role's weight needs no room.
      if (others.length >= MAX_ROLE_WEIGHTS) return failure('LIMIT');
      next = normalizeRoleWeights([...others, { roleId, weight }]);
    } else {
      if (others.length === current.length) return failure('NOT_SET');
      next = others;
    }
    const moved = await prisma.giveaway.updateMany({ where: { id: g.id, status: { in: OPEN_STATUSES } }, data: { roleWeights: next } });
    if (!moved.count) return failure('NOT_OPEN');

    if (g.status === 'ACTIVE' && g.messageId) {
      try {
        const channel = await fetchChannel(guild, g.channelId);
        if (channel) {
          const entryCount = await prisma.giveawayEntry.count({ where: { giveawayId: g.id } });
          const embed = buildGiveawayEmbed({ ...g, roleWeights: next }, { settings, entryCount, roleExists: roleExistsIn(guild) });
          await channel.messages.edit(g.messageId, { embeds: [embed] });
        }
      } catch (err) {
        if (err?.code !== 10008) logger.warn(`Giveaway ${g.id}: could not refresh the embed after an odds change`, err);
      }
    }
    return { ok: true, roleWeights: next };
  });
}

// ── The Participate button ────────────────────────────────────────────────────────

/**
 * Persistent `giveaway:join:{id}` button: toggles entry. The module gate has already run. Never relies on the
 * interaction token beyond the immediate reply.
 */
export async function handleJoinButton(interaction, client) {
  const { params: [action, id] } = parseCustomId(interaction.customId);
  if (action !== 'join' || !id) {
    await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    return;
  }
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const reply = (embed) => safeReply(interaction, { embeds: [embed] });
  if (!interaction.inCachedGuild()) {
    await reply(errorEmbed('Try Again', "I can't check your roles right now. Please try again in a moment."));
    return;
  }

  const g = await prisma.giveaway.findUnique({
    where: { id },
    select: { id: true, guildId: true, status: true, endsAt: true, prize: true, requiredRoleIds: true, deniedRoleIds: true },
  });
  if (!g || g.guildId !== interaction.guildId) {
    await reply(errorEmbed('Giveaway Not Found', 'This giveaway no longer exists.'));
    return;
  }
  if (g.status === 'DRAFT') {
    await reply(warnEmbed('Giveaway Paused', 'This giveaway is not running right now.'));
    return;
  }
  if (g.status !== 'ACTIVE' || g.endsAt.getTime() <= Date.now()) {
    await reply(warnEmbed('Giveaway Closed', 'This giveaway has ended.'));
    return;
  }

  const userId = interaction.user.id;
  const prize = clampText(g.prize, 200);

  // Toggle off. Leaving is always allowed, even after losing a required role.
  const left = await prisma.giveawayEntry.deleteMany({ where: { giveawayId: id, userId } });
  if (left.count > 0) {
    await reply(infoEmbed('Left Giveaway', `You have left the giveaway for **${prize}**.`));
    scheduleCountRefresh(client, id);
    return;
  }

  const guild = interaction.guild;
  const has = (r) => interaction.member.roles.cache.has(r);
  const denied = heldDeniedRole(has, (g.deniedRoleIds ?? []).filter((r) => guild.roles.cache.has(r)));
  if (denied) {
    await reply(errorEmbed('Requirement Not Met', `You can't enter this giveaway with the <@&${denied}> role.`));
    return;
  }
  const missing = (g.requiredRoleIds ?? []).filter((r) => guild.roles.cache.has(r) && !has(r));
  if (missing.length) {
    await reply(errorEmbed('Requirement Not Met', `You need the ${missing.map((r) => `<@&${r}>`).join(', ')} role to enter this giveaway.`));
    return;
  }

  // Toggle on. P2002 is a concurrent double click that already entered: success. P2003: the giveaway was deleted meanwhile.
  try {
    await prisma.giveawayEntry.create({ data: { giveawayId: id, userId } });
  } catch (err) {
    if (err?.code === 'P2003') {
      await reply(errorEmbed('Giveaway Not Found', 'This giveaway no longer exists.'));
      return;
    }
    if (err?.code !== 'P2002') throw err;
  }
  await reply(successEmbed('Entered', `You have entered the giveaway for **${prize}**. Good luck! Click **Participate** again to leave.`));
  scheduleCountRefresh(client, id);
}

// ── The button count ──────────────────────────────────────────────────────────────

/** giveawayId -> { lastAt, timer }: at most one count edit per COUNT_REFRESH_MS per giveaway, trailing edge included. */
const refreshState = new Map();

function pruneRefreshState() {
  const cutoff = Date.now() - 60_000;
  for (const [id, s] of refreshState) if (!s.timer && s.lastAt < cutoff) refreshState.delete(id);
}

/** Clears every pending count refresh (tests and shutdown). */
export function resetCountRefresh() {
  for (const s of refreshState.values()) if (s.timer) clearTimeout(s.timer);
  refreshState.clear();
}

/** Queues a refresh of the Participate (N) label. Synchronous and never throws. */
export function scheduleCountRefresh(client, giveawayId) {
  try {
    let s = refreshState.get(giveawayId);
    if (!s) {
      if (refreshState.size > 500) pruneRefreshState();
      s = { lastAt: 0, timer: null };
      refreshState.set(giveawayId, s);
    }
    if (s.timer) return; // an update is already queued; it will include this click
    const wait = Math.max(0, s.lastAt + COUNT_REFRESH_MS - Date.now());
    s.timer = setTimeout(() => {
      try {
        s.timer = null;
        s.lastAt = Date.now();
        refreshEntryCount(client, giveawayId).catch((err) => logger.warn('Giveaway count refresh failed', err));
      } catch (err) {
        logger.warn('Giveaway count refresh failed', err);
      }
    }, wait);
    s.timer.unref?.();
  } catch (err) {
    logger.warn('scheduleCountRefresh failed', err);
  }
}

/** Edits only the button row. Inside the giveaway lock, so it can never re-enable the button after the end edited it. */
export function refreshEntryCount(client, giveawayId) {
  return locks.run(giveawayId, async () => {
    const g = await prisma.giveaway.findUnique({ where: { id: giveawayId }, select: { guildId: true, channelId: true, messageId: true, status: true } });
    if (g?.status !== 'ACTIVE' || !g.messageId) return;
    const guild = client.guilds.cache.get(g.guildId);
    const channel = guild?.channels.cache.get(g.channelId);
    if (!channel) return;
    const count = await prisma.giveawayEntry.count({ where: { giveawayId } });
    try {
      await channel.messages.edit(g.messageId, { components: [buildJoinRow(giveawayId, count)] });
    } catch (err) {
      if (err?.code === 10008) return; // the messageDelete hook or the end path will draft it
      logger.warn(`Giveaway ${giveawayId}: could not refresh the entry count`, err);
    }
  });
}

// ── Hooks and the job ─────────────────────────────────────────────────────────────

async function draftIfActive(g, messageId) {
  const moved = await prisma.giveaway.updateMany({ where: { id: g.id, status: 'ACTIVE', messageId }, data: { status: 'DRAFT', messageId: null } });
  if (moved.count) {
    await cancelJobs({ dedupeKey: endJobKey(g.id) });
    logger.info(`Giveaway ${g.id} moved to draft (message deleted)`);
  }
  return moved.count > 0;
}

/**
 * messageDelete hook: an ACTIVE giveaway whose message was deleted becomes a DRAFT (its end job is cancelled). On an
 * ENDED or CANCELLED row only the dangling messageId is cleared. Errors propagate to the hook runner, which logs them.
 */
export async function handleGiveawayMessageDelete(ctx) {
  const { guildId, message } = ctx;
  if (!guildId || !message?.id) return;
  const g = await prisma.giveaway.findFirst({ where: { guildId, messageId: message.id }, select: { id: true } });
  if (!g) return;
  await locks.run(g.id, async () => {
    if (await draftIfActive(g, message.id)) return;
    await prisma.giveaway.updateMany({ where: { id: g.id, messageId: message.id, status: { not: 'ACTIVE' } }, data: { messageId: null } });
  });
}

/** messageDeleteBulk hook: the same for every ACTIVE giveaway among the deleted messages, with one batched lookup. */
export async function handleGiveawayBulkDelete(ctx) {
  const { guildId } = ctx;
  const ids = [...(ctx.messages?.keys?.() ?? [])].map(String);
  if (!guildId || !ids.length) return;
  const rows = await prisma.giveaway.findMany({ where: { guildId, messageId: { in: ids }, status: 'ACTIVE' }, select: { id: true, messageId: true } });
  for (const row of rows) {
    try {
      await locks.run(row.id, () => draftIfActive(row, row.messageId));
    } catch (err) {
      logger.error(`Giveaway ${row.id}: bulk-delete handling failed`, err);
    }
  }
}

/**
 * `giveaway.end` job handler. Retries come from the throws in endGiveaway (the scheduler backs off, 5 attempts); on the
 * final attempt endGiveaway stops retrying and ends with what it can do.
 */
export async function handleEndJob(job, ctx) {
  const giveawayId = job.payload?.giveawayId;
  if (typeof giveawayId !== 'string' || !giveawayId) throw new PermanentJobError('giveaway.end: payload.giveawayId missing');
  const res = await endGiveaway({ client: ctx.client, guild: ctx.guild, giveawayId, source: 'job', attempt: ctx.attempt });
  if (res && res.code === 'NOT_DUE') return { nextRunAt: res.nextRunAt };
  return undefined; // every other result (ok, NOT_FOUND, NOT_ACTIVE, DRAFTED, final-attempt paths) completes the job
}
