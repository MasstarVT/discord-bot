// Starboards (pkg-starboard.md): pure helpers, the per-guild board cache, the CRUD behind /starboard and the sync engine that the
// plugin hooks drive. Side-effect free at import: no timers, no I/O, no registration.
import {
  ActionRowBuilder, ButtonBuilder, ButtonStyle, Collection, EmbedBuilder, ReactionType, StickerFormatType,
} from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma, { SQL_NOW } from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings } from './settingsService.js';
import { LIMITS } from '../config/constants.js';
import { createKeyedMutex, createThrottle, mapLimit, sleep } from '../utils/async.js';
import { markBotDeletion, wasDeletedByBot } from '../utils/deletionTracker.js';
import { clampText, formatNumber, joinWithLimit } from '../utils/text.js';
import { checkChannelUsable } from '../utils/interactionHelpers.js';
import logger from '../utils/logger.js';

export const STAR_COLOR = 0xFFAC33;
export const MAX_EMOJIS_PER_BOARD = 10;
export const MAX_IGNORED_CHANNELS = 50;
export const MAX_USER_PAGES = 10;          // 100 users per page -> at most 1000 users fetched per reaction
export const SYNC_CONCURRENCY = 4;         // concurrent syncs per shard
// A message that was synced a moment ago waits this long before its next sync, so a burst of reactions on one hot message
// produces a few count edits instead of one per reaction (Discord allows about 5 edits per 5 s per channel).
export const SYNC_SPACING_MS = 1500;
export const BOARD_MEMO_TTL_MS = 60_000;
const BOARD_CACHE_TTL_S = 300;
const BOARD_MEMO_MAX = 10_000;
const DAY_MS = 86_400_000;
const SNOWFLAKE_EPOCH = 1420070400000n;
const COUNT_MAX = 1_000_000_000;
// Characters /starboard list may spend on field values, which keeps the embed under Discord's 6000-character total.
export const LIST_EMBED_BUDGET = 5600;

export class BoardLimitError extends Error {
  constructor(message = `A server can have at most ${LIMITS.STARBOARDS} starboards.`) { super(message); this.name = 'BoardLimitError'; }
}
export class NotFoundError extends Error {
  constructor(message = 'Starboard not found.') { super(message); this.name = 'NotFoundError'; }
}
export class IgnoreLimitError extends Error {
  constructor(message = `A starboard can ignore at most ${MAX_IGNORED_CHANNELS} channels.`) { super(message); this.name = 'IgnoreLimitError'; }
}
export class SettingsConflictError extends Error {
  constructor(message = 'Self-stars are counted on this board, so they cannot also be removed.') { super(message); this.name = 'SettingsConflictError'; }
}

/** @returns {boolean} true for a Prisma unique-constraint violation (P2002) */
export const isUniqueViolation = (err) => err instanceof Prisma.PrismaClientKnownRequestError && err.code === 'P2002';

const errCode = (err) => err?.code ?? err?.rawError?.code;
const isNoAccess = (err) => errCode(err) === 50001 || errCode(err) === 50013;
const isGone = (err) => errCode(err) === 10008 || errCode(err) === 10003;
const valuesOf = (collection) => (typeof collection?.values === 'function' ? collection.values() : (collection ?? []));

const allowBoardWarn = createThrottle(60_000);
const allowPermWarn = createThrottle(600_000);
const allowReadWarn = createThrottle(600_000);

export const logSyncError = (err) => logger.error('starboard sync failed', err instanceof Error ? err : new Error(String(err)));

/** logger.warn throttled per board and error code (10 minutes). */
function warnOnce(board, code, msg) {
  if (allowPermWarn(`${board.id}:${code}`)) logger.warn(msg);
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** @returns {string|null} lowercase [a-z0-9_-], 1-32 characters, or null */
export function normalizeBoardName(input) {
  const n = String(input ?? '').trim().toLowerCase().replace(/\s+/g, '-').replace(/[^a-z0-9_-]/g, '')
    .replace(/-{2,}/g, '-').replace(/^-+|-+$/g, '').slice(0, 32);
  return n.length ? n : null;
}

/** Strips the emoji variation selector, so '❤' and '❤️' are one emoji. */
export const normalizeEmojiKey = (key) => String(key ?? '').replace(/️/g, '');

/** The key the dispatchers put in `ctx.emojiKey`: the custom emoji id, or the unicode emoji itself. */
export const emojiKeyOf = (emoji) => emoji?.id ?? emoji?.name;

export function boardMatchesEmoji(board, key) {
  if (!board.emojis || board.emojis.length === 0) return true;
  if (key == null) return false;
  const k = normalizeEmojiKey(key);
  return board.emojis.some((e) => normalizeEmojiKey(e) === k);
}

// ️ = variation selector-16, ⃣ = combining keycap, ‍ = zero-width joiner (written as escapes on purpose).
const UNICODE_EMOJI_RE = /^(?:\p{Regional_Indicator}{2}|[#*0-9]️?⃣|(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:️|\p{Emoji_Modifier})?(?:[\u{E0020}-\u{E007E}]+\u{E007F})?(?:‍(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:️|\p{Emoji_Modifier})?)*)$/u;

/** @returns {boolean} true when one grapheme is a unicode emoji */
export const isUnicodeEmoji = (grapheme) => UNICODE_EMOJI_RE.test(String(grapheme ?? ''));

let segmenter = null;
const getSegmenter = () => (segmenter ??= new Intl.Segmenter('en', { granularity: 'grapheme' }));

function findGuildEmoji(guild, name) {
  const lower = name.toLowerCase();
  let loose = null;
  for (const e of valuesOf(guild?.emojis?.cache)) {
    if (e.name === name) return e;
    if (!loose && String(e.name).toLowerCase() === lower) loose = e;
  }
  return loose;
}

/**
 * Parses the `emoji` option: unicode emojis, custom emojis (`<:name:id>`, `:name:` or a bare id) or "any".
 * @returns {{ ok: true, emojis: string[] } | { ok: false, error: string }}
 */
export function parseEmojiList(input, guild) {
  const s = String(input ?? '').trim();
  const empty = { ok: false, error: 'Give at least one emoji, or "any" for every emoji.' };
  if (!s) return empty;
  if (/^(any|all|\*)$/i.test(s)) return { ok: true, emojis: [] };

  const tokens = s.replace(/<a?:\w{1,32}:(\d{17,20})>/g, ' $1 ').split(/[\s,]+/).filter(Boolean);
  const found = [];
  for (const token of tokens) {
    if (/^\d{17,20}$/.test(token)) { found.push(token); continue; }
    const named = /^:(\w{1,32}):$/.exec(token);
    if (named) {
      const emoji = findGuildEmoji(guild, named[1]);
      if (!emoji) return { ok: false, error: `I can't find a server emoji called :${named[1]}:. Use the emoji itself.` };
      found.push(emoji.id);
      continue;
    }
    for (const { segment } of getSegmenter().segment(token)) {
      if (!isUnicodeEmoji(segment)) return { ok: false, error: `"${clampText(token, 40)}" is not an emoji.` };
      found.push(segment);
    }
  }
  if (found.length === 0) return empty;

  const seen = new Set();
  const emojis = [];
  for (const e of found) {
    const k = normalizeEmojiKey(e);
    if (seen.has(k)) continue;
    seen.add(k);
    emojis.push(e);
  }
  if (emojis.length > MAX_EMOJIS_PER_BOARD) return { ok: false, error: `A starboard can use at most ${MAX_EMOJIS_PER_BOARD} emojis.` };
  return { ok: true, emojis };
}

/** Renders an emoji key: unicode as is, an id as the custom emoji when it can be resolved (else a star). */
export function formatEmojiKey(key, { client, message } = {}) {
  if (!/^\d{17,20}$/.test(String(key))) return String(key);
  for (const r of valuesOf(message?.reactions?.cache)) {
    if (r.emoji?.id === key) return r.emoji.toString();
  }
  return client?.emojis?.cache?.get(key)?.toString() ?? '⭐';
}

export function formatEmojiList(keys, client) {
  return keys && keys.length ? keys.map((k) => formatEmojiKey(k, { client })).join(' ') : 'any emoji';
}

/** Creation time (ms) of a snowflake. Works on partial messages. */
export const snowflakeTime = (id) => Number((BigInt(id) >> 22n) + SNOWFLAKE_EPOCH);

/** The channel, its parent channel or category, and a thread's category. */
export function channelChain(channel) {
  return [channel.id, channel.parentId, channel.isThread?.() ? channel.parent?.parentId : null].filter(Boolean);
}

export const isNsfwChannel = (channel) => (channel.isThread?.() ? !!channel.parent?.nsfw : !!channel.nsfw);

/** @returns {null|'board-channel'|'ignored'|'nsfw'} why a channel cannot feed this board */
export function sourceSkipReason({ board, channel, boardChannelIds, boardChannel }) {
  const chain = channelChain(channel);
  if (chain.some((id) => boardChannelIds.has(id))) return 'board-channel';
  if (chain.some((id) => (board.ignoredChannelIds ?? []).includes(id))) return 'ignored';
  if (isNsfwChannel(channel) && !(board.allowNsfw && boardChannel?.nsfw === true)) return 'nsfw';
  return null;
}

/** @returns {null|'system'|'own-bot'|'bot-author'|'too-old'} why a message cannot be reposted */
export function messageSkipReason({ board, message, hasEntry, now = Date.now(), botUserId }) {
  if (message.system === true) return 'system';
  if (message.author?.id === botUserId) return 'own-bot';
  if (!hasEntry && board.ignoreBots && (message.author?.bot || message.webhookId)) return 'bot-author';
  if (!hasEntry && board.maxMessageAgeDays > 0 && now - snowflakeTime(message.id) > board.maxMessageAgeDays * DAY_MS) return 'too-old';
  return null;
}

/**
 * Number of distinct users across a board's qualifying reactions, after exclusions.
 * @param {{ board: object, userLists: Array<{ users: Iterable<{ id: string, bot?: boolean }>, extra?: number }>, authorId?: string, botUserId?: string }} p
 */
export function computeCount({ board, userLists, authorId, botUserId }) {
  const union = new Map();
  let extra = 0;
  for (const list of userLists) {
    for (const u of valuesOf(list.users)) union.set(u.id, u);
    extra += Number(list.extra) || 0;
  }
  let total = 0;
  for (const [id, u] of union) {
    if (id === botUserId) continue;
    if (board.ignoreBots && u.bot) continue;
    if (!board.selfStar && id === authorId) continue;
    total++;
  }
  return Math.min(total + extra, COUNT_MAX);
}

/** @returns {'create'|'repost'|'update'|'remove'|'store'|'none'} what to do for one (board, message) */
export function decideAction({ entry, count, board }) {
  if (!entry) return count >= board.threshold ? 'create' : 'none';
  // A repost that staff deleted by hand is never reposted (checked before the dropped rows: it has no boardMessageId either).
  if (entry.blocked) return count !== entry.count ? 'store' : 'none';
  if (entry.boardMessageId) {
    if (count < board.threshold && board.removeBelowThreshold) return 'remove';
    return count !== entry.count ? 'update' : 'none';
  }
  if (count >= board.threshold) return 'repost';
  return count !== entry.count ? 'store' : 'none';
}

export function pickDisplayEmoji({ board, qualifying, client, message }) {
  if (board.emojis && board.emojis.length) return formatEmojiKey(board.emojis[0], { client, message });
  let best = null;
  for (const r of qualifying ?? []) {
    if (best === null || (r.count ?? 0) > (best.count ?? 0)) best = r;
  }
  return best ? best.emoji.toString() : '⭐';
}

export const buildCountLine = ({ emojiText, count, sourceChannelId }) => `${emojiText} **${formatNumber(count)}** · <#${sourceChannelId}>`;

const isSpoiler = (att) => att.spoiler === true || String(att.name ?? '').startsWith('SPOILER_');
const isImageAttachment = (att) => String(att.contentType ?? '').startsWith('image/') || /\.(png|jpe?g|gif|webp)$/i.test(att.name ?? '');

/** @returns {{ url: string, attachmentId: string|null }|null} the first usable image of a message */
export function selectImage(message) {
  for (const att of valuesOf(message.attachments)) {
    if (!isSpoiler(att) && isImageAttachment(att)) return { url: att.url, attachmentId: att.id };
  }
  for (const embed of message.embeds ?? []) {
    if (embed.image?.url) return { url: embed.image.url, attachmentId: null };
    const type = embed.data?.type ?? embed.type;
    if ((type === 'image' || type === 'gifv') && embed.thumbnail?.url) return { url: embed.thumbnail.url, attachmentId: null };
  }
  for (const sticker of valuesOf(message.stickers)) {
    if (sticker.format !== StickerFormatType.Lottie && sticker.url) return { url: sticker.url, attachmentId: null };
  }
  return null;
}

/**
 * The repost message for a source message. Mirrors user content, so it carries the source's timestamp and no title
 * (the embedBuilder factories force a title and the current time).
 */
export function buildRepostPayload({ message, count, emojiText }) {
  const author = message.member ?? message.author;
  const name = clampText(message.member?.displayName ?? message.author?.displayName ?? message.author?.username, 256) || 'Unknown';
  const iconURL = typeof author?.displayAvatarURL === 'function' ? author.displayAvatarURL({ extension: 'png', size: 128 }) : undefined;

  const embed = new EmbedBuilder().setColor(STAR_COLOR).setAuthor(iconURL ? { name, iconURL } : { name });

  let text = message.content ?? '';
  if (!text && message.embeds?.[0]) text = message.embeds[0].description ?? message.embeds[0].title ?? '';
  for (const sticker of valuesOf(message.stickers)) text = `${text}\n\nSticker: **${sticker.name}**`.trim();
  if (text) embed.setDescription(clampText(text, 4096));

  const img = selectImage(message);
  if (img) embed.setImage(img.url);

  const items = [];
  for (const att of valuesOf(message.attachments)) {
    if (img && att.id === img.attachmentId) continue;
    const label = clampText(String(att.name ?? 'file').replace(/[[\]]/g, ''), 80) || 'file';
    const link = `[${label}](${att.url})`;
    items.push(isSpoiler(att) ? `||${link}||` : link);
  }
  if (items.length) embed.addFields({ name: 'Attachments', value: joinWithLimit(items, { sep: '\n', max: 1024 }) });

  embed.setTimestamp(message.createdTimestamp);

  const row = new ActionRowBuilder().addComponents(
    new ButtonBuilder().setStyle(ButtonStyle.Link).setLabel('Jump to message').setURL(message.url),
  );
  return {
    content: clampText(buildCountLine({ emojiText, count, sourceChannelId: message.channelId }), 2000),
    embeds: [embed],
    components: [row],
    allowedMentions: { parse: [] },
  };
}

/** @returns {{ run: <T>(fn: () => Promise<T>|T) => Promise<T>, active: number, queued: number }} a counting semaphore */
export function createSemaphore(limit) {
  let active = 0;
  const queue = [];
  const release = () => {
    active--;
    const next = queue.shift();
    if (next) next();
  };
  const acquire = () => new Promise((resolve) => {
    const take = () => { active++; resolve(); };
    if (active < limit) take();
    else queue.push(take);
  });
  return {
    async run(fn) {
      await acquire();
      try {
        return await fn();
      } finally {
        release();
      }
    },
    get active() { return active; },
    get queued() { return queue.length; },
  };
}

/**
 * Per key at most one running sync and one waiting sync: a request while one is already waiting returns null, because the
 * waiting run reads the newest Discord state anyway. Lock order is always mutex, then semaphore. `spacingMs` makes a run
 * wait until that long after the previous run for the same key ended (burst coalescing).
 */
export function createCoalescer({ mutex, semaphore, spacingMs = 0 }) {
  const waiting = new Set();
  const lastEnd = new Map();
  const sweep = (now) => {
    for (const [k, t] of lastEnd) if (now - t >= spacingMs) lastEnd.delete(k);
  };
  function request(key, fn) {
    if (waiting.has(key)) return null;
    waiting.add(key);
    return mutex.run(key, async () => {
      if (spacingMs > 0) {
        const wait = (lastEnd.get(key) ?? 0) + spacingMs - Date.now();
        if (wait > 0) await sleep(wait);
      }
      try {
        return await semaphore.run(async () => { waiting.delete(key); return fn(); });
      } finally {
        if (spacingMs > 0) {
          const now = Date.now();
          lastEnd.set(key, now);
          if (lastEnd.size > 500) sweep(now);
        }
      }
    });
  }
  return { request, waitingSize: () => waiting.size };
}

const mutex = createKeyedMutex();
const semaphore = createSemaphore(SYNC_CONCURRENCY);
const coalescer = createCoalescer({ mutex, semaphore, spacingMs: SYNC_SPACING_MS });

// ── RAW SQL ───────────────────────────────────────────────────────────────────

const lockGuildSettingsRow = (guildId) => Prisma.sql`SELECT 1 FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`;
const lockBoardRow = (boardId, guildId) => Prisma.sql`SELECT 1 FROM "Starboard" WHERE "id" = ${boardId} AND "guildId" = ${guildId} FOR UPDATE`;
const removeIgnoredChannelSql = (guildId, channelId) => Prisma.sql`UPDATE "Starboard" SET "ignoredChannelIds" = array_remove("ignoredChannelIds", ${channelId}), "updatedAt" = ${SQL_NOW}
  WHERE "guildId" = ${guildId} AND ${channelId} = ANY("ignoredChannelIds")`;

/** One entry per raw statement, with sample parameters (01-foundation §1). */
export const RAW_SQL = Object.freeze([
  { name: 'starboard.lockGuildSettings', build: () => lockGuildSettingsRow('0') },
  { name: 'starboard.lockBoard', build: () => lockBoardRow('0', '0') },
  { name: 'starboard.removeIgnoredChannel', build: () => removeIgnoredChannelSql('0', '0') },
]);

// ── The board cache ───────────────────────────────────────────────────────────

const BOARD_SELECT = {
  id: true, name: true, channelId: true, emojis: true, threshold: true, selfStar: true, ignoreBots: true,
  removeBelowThreshold: true, autoReact: true, maxMessageAgeDays: true, allowNsfw: true, ignoredChannelIds: true,
  removeSelfStars: true, enabled: true,
};
const boardsKey = (guildId) => `guild:${guildId}:starboards`;

/** @type {Map<string, { at: number, boards: object[] }>} insertion order = age */
const boardMemo = new Map();
/** guildId -> number, bumped by invalidateBoards so a slow loader never memoises a stale list. */
const generations = new Map();
const generation = (guildId) => generations.get(guildId) ?? 0;

function memoSet(guildId, boards) {
  boardMemo.delete(guildId);
  boardMemo.set(guildId, { at: Date.now(), boards });
  while (boardMemo.size > BOARD_MEMO_MAX) boardMemo.delete(boardMemo.keys().next().value);
}

/** Primitives only: no dates, so the list survives a JSON round trip through Redis. */
function toBoardLite(row) {
  if (!row || typeof row !== 'object' || typeof row.id !== 'string' || typeof row.channelId !== 'string') return null;
  return {
    id: row.id,
    name: String(row.name ?? ''),
    channelId: row.channelId,
    emojis: Array.isArray(row.emojis) ? row.emojis.map(String) : [],
    threshold: Number.isInteger(row.threshold) ? row.threshold : 3,
    selfStar: row.selfStar === true,
    ignoreBots: row.ignoreBots !== false,
    removeBelowThreshold: row.removeBelowThreshold !== false,
    autoReact: row.autoReact === true,
    maxMessageAgeDays: Number.isInteger(row.maxMessageAgeDays) ? row.maxMessageAgeDays : 0,
    allowNsfw: row.allowNsfw === true,
    ignoredChannelIds: Array.isArray(row.ignoredChannelIds) ? row.ignoredChannelIds.map(String) : [],
    removeSelfStars: row.removeSelfStars === true,
    enabled: row.enabled !== false,
  };
}

/** Synchronous, memo-only. @returns {object[]|null} null when the guild is unknown or its entry expired */
export function peekBoards(guildId) {
  const entry = boardMemo.get(guildId);
  if (!entry) return null;
  if (Date.now() - entry.at >= BOARD_MEMO_TTL_MS) {
    boardMemo.delete(guildId);
    return null;
  }
  return entry.boards;
}

/**
 * The boards of a guild. Never throws. Redis down -> fail OPEN to Postgres; Postgres down -> null, and every caller then
 * SKIPS the event (fail CLOSED: no starboard action without knowing the configuration).
 * @returns {Promise<object[]|null>}
 */
export async function getBoards(guildId, { fresh = false } = {}) {
  try {
    // A list read while invalidateBoards ran is returned to this caller but never memoised.
    const gen = generation(guildId);
    if (!fresh) {
      const memo = peekBoards(guildId);
      if (memo) return memo;
      const hit = await cache.get(boardsKey(guildId));
      if (Array.isArray(hit)) {
        const boards = hit.map(toBoardLite).filter(Boolean);
        if (generation(guildId) === gen) memoSet(guildId, boards);
        return boards;
      }
    }
    const rows = await prisma.starboard.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' }, select: BOARD_SELECT });
    const boards = rows.map(toBoardLite).filter(Boolean);
    if (generation(guildId) === gen) {
      await cache.set(boardsKey(guildId), boards, BOARD_CACHE_TTL_S);
      memoSet(guildId, boards);
    }
    return boards;
  } catch (err) {
    if (allowBoardWarn(guildId)) logger.warn(`Starboard: could not load the boards of guild ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

/** Drops the memo entry and the Redis list. Call after every create, edit, delete and disable. */
export async function invalidateBoards(guildId) {
  boardMemo.delete(guildId);
  generations.set(guildId, generation(guildId) + 1);
  if (generations.size > BOARD_MEMO_MAX * 2) generations.delete(generations.keys().next().value);
  await cache.del(boardsKey(guildId));
}

/** Hook filter: false only when the memo says this guild has no boards at all. An unknown memo returns true. */
export const guildMayHaveBoards = (ctx) => !!ctx.guildId && peekBoards(ctx.guildId)?.length !== 0;

/** Test-only: primes (or, with null, clears) the in-process memo. */
export function __setBoardsMemo(guildId, boards) {
  if (boards == null) boardMemo.delete(guildId);
  else boardMemo.set(guildId, { at: Date.now(), boards });
}

// ── Counting qualifying reactions ─────────────────────────────────────────────

/**
 * Every distinct user of one reaction (normal plus super reactions), at most MAX_USER_PAGES pages. `extra` is the
 * unverified remainder of a truncated list. Errors propagate to the sync wrapper.
 * @returns {Promise<{ users: Collection<string, any>, extra: number }>}
 */
export async function fetchReactionUsers(reaction) {
  const types = [ReactionType.Normal];
  if ((reaction.countDetails?.burst ?? 0) > 0) types.push(ReactionType.Burst);
  const users = new Collection();
  let pages = 0;
  let truncated = false;
  for (const type of types) {
    let after;
    for (;;) {
      if (pages >= MAX_USER_PAGES) { truncated = true; break; }
      const batch = await reaction.users.fetch({ type, limit: 100, after });
      pages++;
      for (const [id, u] of batch) users.set(id, u);
      if (batch.size < 100) break;
      after = [...batch.keys()].reduce((a, b) => (BigInt(b) > BigInt(a) ? b : a));
    }
    if (truncated) break;
  }
  return { users, extra: truncated ? Math.max(0, (reaction.count ?? 0) - users.size) : 0 };
}

// ── Sync engine ───────────────────────────────────────────────────────────────

/** Boards that are on and may be fed by `channel`. Board channels are taken from ALL boards (disabled ones included). */
function eligibleBoards(boards, channel, guild) {
  const boardChannelIds = new Set(boards.map((b) => b.channelId));
  return boards.filter((b) => b.enabled
    && sourceSkipReason({ board: b, channel, boardChannelIds, boardChannel: guild.channels.cache.get(b.channelId) }) === null);
}

async function disableBoards(guildId, boardIds, channelId) {
  await prisma.$transaction([
    prisma.starboard.updateMany({ where: { id: { in: boardIds }, guildId }, data: { enabled: false } }),
    prisma.starboardEntry.updateMany({ where: { starboardId: { in: boardIds } }, data: { boardMessageId: null } }),
  ]);
  await invalidateBoards(guildId);
  logger.info(`Starboard(s) ${boardIds.join(', ')} disabled: channel ${channelId} is gone (${guildId})`);
}

/** The board's channel from the cache, else a REST fetch. 10003 from REST disables the board (rule 20: never on a cache miss). */
async function resolveBoardChannel(guild, board) {
  const cached = guild.channels.cache.get(board.channelId);
  if (cached) return cached;
  try {
    const channel = await guild.client.channels.fetch(board.channelId);
    return channel?.guildId === guild.id ? channel : null;
  } catch (err) {
    if (errCode(err) === 10003 && guild.available) {
      try {
        await disableBoards(guild.id, [board.id], board.channelId);
      } catch (dbErr) {
        logger.error('Starboard: could not disable a board whose channel is gone', dbErr);
      }
    }
    return null;
  }
}

/** A repost that staff deleted by hand (or that was purged): never reposted again. */
const blockEntry = (entry, boardMessageId) => prisma.starboardEntry.updateMany({
  where: { id: entry.id, boardMessageId },
  data: { boardMessageId: null, blocked: true },
});

/**
 * Edits a live repost. 10008 (deleted by hand) blocks the entry, 50001/50013 warn. Returns false when the repost is gone, so
 * the caller skips its database update.
 */
async function editRepost({ guild, board, entry, bc, payload }) {
  try {
    await bc.messages.edit(entry.boardMessageId, payload);
    return true;
  } catch (err) {
    if (errCode(err) === 10008) {
      if (guild.available) await blockEntry(entry, entry.boardMessageId);
      return false;
    }
    if (isNoAccess(err)) {
      warnOnce(board, 'perm', `Starboard "${board.name}": cannot edit reposts in <#${board.channelId}> (${guild.id}); give me View Channel, Send Messages and Embed Links there`);
      return true;
    }
    throw err;
  }
}

const REPOST_NEEDS = ['ViewChannel', 'SendMessages', 'EmbedLinks'];

function reactIdentifier(board, client, qualifying) {
  if (board.emojis.length === 0) {
    let best = null;
    for (const r of qualifying) if (best === null || (r.count ?? 0) > (best.count ?? 0)) best = r;
    return best?.emoji ?? null;
  }
  const first = board.emojis[0];
  if (!/^\d{17,20}$/.test(first)) return first;
  return client.emojis.cache.get(first) ?? qualifying.find((r) => r.emoji?.id === first)?.emoji ?? null;
}

/** Applies one decided action. Discord and database errors are handled per §6.5; anything unexpected propagates. */
async function applyAction({ action, board, entry, message, count, emojiText, guild, client, qualifying = [] }) {
  if (action === 'none') return;
  if (action === 'store') {
    await prisma.starboardEntry.updateMany({ where: { id: entry.id }, data: { count } });
    return;
  }

  const bc = await resolveBoardChannel(guild, board);
  if (!bc) return;

  if (action === 'create' || action === 'repost') {
    const usable = checkChannelUsable(bc, { needs: REPOST_NEEDS });
    if (!usable.ok) {
      // The entry is not created, so the next event retries once the permissions are fixed.
      warnOnce(board, 'perm', `Starboard "${board.name}": cannot post in <#${board.channelId}> (${guild.id}): ${usable.reason ?? 'missing permissions'}`);
      return;
    }
    let sent;
    try {
      sent = await bc.send(buildRepostPayload({ message, count, emojiText }));
    } catch (err) {
      if (isNoAccess(err)) {
        warnOnce(board, 'perm', `Starboard "${board.name}": cannot post in <#${board.channelId}> (${guild.id})`);
        return;
      }
      throw err;
    }

    let saved = false;
    try {
      if (action === 'create') {
        await prisma.starboardEntry.create({
          data: {
            starboardId: board.id, sourceChannelId: message.channelId, sourceMessageId: message.id,
            authorId: message.author.id, boardMessageId: sent.id, count,
          },
        });
        saved = true;
      } else {
        const r = await prisma.starboardEntry.updateMany({ where: { id: entry.id, boardMessageId: null, blocked: false }, data: { boardMessageId: sent.id, count } });
        saved = r.count > 0;
        if (!saved) logger.debug(`Starboard: repost of ${message.id} conflicted, removing the extra copy`);
      }
    } catch (err) {
      if (isUniqueViolation(err)) logger.debug(`Starboard: entry for ${message.id} already exists, removing the extra copy`);
      else logger.error('Starboard: could not save an entry, removing the repost', err);
    }
    if (!saved) {
      markBotDeletion(sent.id, 'starboard');
      await sent.delete().catch(() => {});
      return;
    }

    if (board.autoReact) {
      const identifier = reactIdentifier(board, client, qualifying);
      if (identifier) {
        await sent.react(identifier).catch((err) => warnOnce(board, 'react', `Starboard "${board.name}": could not add the auto-reaction (${err?.code ?? err?.message})`));
      }
    }
    return;
  }

  if (action === 'update') {
    // Only the content line changes; the embed and the button stay as posted.
    const payload = { content: buildCountLine({ emojiText, count, sourceChannelId: message.channelId }), allowedMentions: { parse: [] } };
    if (!(await editRepost({ guild, board, entry, bc, payload }))) return;
    await prisma.starboardEntry.updateMany({ where: { id: entry.id }, data: { count } });
    return;
  }

  if (action === 'remove') {
    markBotDeletion(entry.boardMessageId, 'starboard');
    try {
      await bc.messages.delete(entry.boardMessageId);
    } catch (err) {
      if (isNoAccess(err)) {
        warnOnce(board, 'perm', `Starboard "${board.name}": cannot delete reposts in <#${board.channelId}> (${guild.id}); give me View Channel there`);
        await prisma.starboardEntry.updateMany({ where: { id: entry.id }, data: { count } });
        return;
      }
      if (errCode(err) !== 10008) throw err;
    }
    // Dropped: reposted again when the count reaches the threshold.
    await prisma.starboardEntry.updateMany({ where: { id: entry.id, boardMessageId: entry.boardMessageId }, data: { boardMessageId: null, count } });
  }
}

/** Reads of the message or its reactions failed: 10008/10003 end the run silently, 50001/50013 with a throttled warning. */
function readFailureIsBenign(err, channel, guild) {
  if (isGone(err)) return true;
  if (isNoAccess(err)) {
    if (allowReadWarn(channel.id)) logger.warn(`Starboard: no access to read reactions in #${channel.name} (${guild.id})`);
    return true;
  }
  return false;
}

/** Test-only: replaces the sync the handlers enqueue. */
let syncImpl = null;
export function __setSyncImpl(fn) { syncImpl = fn ?? null; }

/**
 * Recounts one message for every eligible board and applies the result. Runs under the mutex and semaphore (see
 * `coalescer`); every run re-evaluates ALL eligible boards, so a coalesced event never loses a board.
 */
export async function syncMessage({ client, guild, channel, messageId, ensureMessage }) {
  const boards = await getBoards(guild.id);
  if (!boards) return;
  let candidates = eligibleBoards(boards, channel, guild);
  if (!candidates.length) return;

  const entries = await prisma.starboardEntry.findMany({ where: { sourceMessageId: messageId, starboardId: { in: candidates.map((b) => b.id) } } });
  const byBoard = new Map(entries.map((e) => [e.starboardId, e]));

  // Age pre-check, before any fetch: a message with no entry that is already too old can never be reposted.
  const now = Date.now();
  const born = snowflakeTime(messageId);
  candidates = candidates.filter((b) => byBoard.has(b.id) || !(b.maxMessageAgeDays > 0 && now - born > b.maxMessageAgeDays * DAY_MS));
  if (!candidates.length) return;

  let message = channel.messages.cache.get(messageId);
  if (!message || message.partial) {
    try {
      message = ensureMessage ? await ensureMessage() : await channel.messages.fetch(messageId);
    } catch (err) {
      if (readFailureIsBenign(err, channel, guild)) return;
      throw err;
    }
  }
  // Nothing to do for an inaccessible or deleted source: messageDelete handles deletions.
  if (!message || message.partial || !message.author) return;

  const botUserId = client.user.id;
  const userMemo = new Map();
  const usersOf = (reaction) => {
    const key = normalizeEmojiKey(emojiKeyOf(reaction.emoji));
    let promise = userMemo.get(key);
    if (!promise) {
      promise = fetchReactionUsers(reaction);
      userMemo.set(key, promise);
    }
    return promise;
  };

  let firstError = null;
  for (const board of candidates) {
    try {
      const entry = byBoard.get(board.id) ?? null;
      // A blocked entry never changes again, so it needs no recount.
      if (entry?.blocked) continue;
      if (messageSkipReason({ board, message, hasEntry: !!entry, botUserId })) continue;

      const qualifying = [...message.reactions.cache.values()].filter((r) => boardMatchesEmoji(board, emojiKeyOf(r.emoji)));
      // Exclusions only lower the count, so a board whose raw total is below the threshold cannot create a repost.
      const rawTotal = qualifying.reduce((sum, r) => sum + (r.count ?? 0), 0);
      if (!entry && rawTotal < board.threshold) continue;

      let count;
      if (board.selfStar && !board.ignoreBots && qualifying.length === 1) {
        count = Math.max(0, (qualifying[0].count ?? 0) - (qualifying[0].me ? 1 : 0));
      } else {
        let userLists;
        try {
          userLists = await Promise.all(qualifying.map(usersOf));
        } catch (err) {
          if (readFailureIsBenign(err, channel, guild)) return;
          throw err;
        }
        count = computeCount({ board, userLists, authorId: message.author.id, botUserId });
      }

      const action = decideAction({ entry, count, board });
      const emojiText = pickDisplayEmoji({ board, qualifying, client, message });
      await applyAction({ action, board, entry, message, count, emojiText, guild, client, qualifying });
    } catch (err) {
      firstError ??= err;
    }
  }
  if (firstError) throw firstError;
}

// ── Hook: reactions ───────────────────────────────────────────────────────────

/** Hook filter (sync): false when no enabled board can care about this emoji. RemoveAll has no emojiKey. */
export function reactionFilter(ctx) {
  if (!ctx.guildId) return false;
  const boards = peekBoards(ctx.guildId);
  if (boards === null) return true;
  return boards.some((b) => b.enabled && (ctx.emojiKey == null || boardMatchesEmoji(b, ctx.emojiKey)));
}

/** MEE6 "remove self-stars": takes the author's own qualifying reaction away. Needs Manage Messages; never counts either way. */
async function removeSelfStar(ctx, channel, boards) {
  const message = ctx.message.partial ? await ctx.ensureMessage?.() : ctx.message;
  if (!message?.author || message.author.id !== ctx.user.id) return;
  const guard = boards.filter((b) => b.removeSelfStars && boardMatchesEmoji(b, ctx.emojiKey));
  if (!guard.length) return;
  if (!checkChannelUsable(channel, { needs: ['ViewChannel', 'ManageMessages'] }).ok) {
    warnOnce(guard[0], 'selfstar', `Starboard "${guard[0].name}": I need Manage Messages in #${channel.name} (${ctx.guild.id}) to remove self-stars`);
    return;
  }
  try {
    await ctx.reaction.users.remove(ctx.user.id);
  } catch (err) {
    if (isNoAccess(err)) warnOnce(guard[0], 'selfstar', `Starboard "${guard[0].name}": could not remove a self-star in #${channel.name} (${ctx.guild.id}): missing Manage Messages`);
    else if (errCode(err) !== 10008) throw err;
  }
}

/**
 * The reaction hook body. Resolves once the sync is queued (never waits for Discord). @returns {Promise<string>} what happened:
 * 'skipped' (an early exit), 'self' (the author's own reaction cannot change a count), 'coalesced' or 'queued'.
 */
export async function handleReaction(ctx) {
  const boards = await getBoards(ctx.guildId);
  if (!boards) return 'skipped';
  const active = boards.filter((b) => b.enabled && (ctx.emojiKey == null || boardMatchesEmoji(b, ctx.emojiKey)));
  if (!active.length) return 'skipped';

  let channel = ctx.message.channel ?? ctx.guild.channels.cache.get(ctx.message.channelId);
  if (!channel) channel = await ctx.client.channels.fetch(ctx.message.channelId).catch(() => null);
  if (!channel) return 'skipped';

  const boardChannelIds = new Set(boards.map((b) => b.channelId));
  const candidates = active.filter((b) => sourceSkipReason({
    board: b, channel, boardChannelIds, boardChannel: ctx.guild.channels.cache.get(b.channelId),
  }) === null);
  if (!candidates.length) return 'skipped';

  if (ctx.event === 'messageReactionAdd' && ctx.user && ctx.reaction && candidates.some((b) => b.removeSelfStars)) {
    removeSelfStar(ctx, channel, candidates).catch(logSyncError);
  }

  // The author's own reaction cannot change the count when no candidate counts self-stars.
  if ((ctx.event === 'messageReactionAdd' || ctx.event === 'messageReactionRemove') && ctx.user && !ctx.message.partial
    && ctx.message.author?.id === ctx.user.id && candidates.every((b) => !b.selfStar)) return 'self';

  // Steps above are early exits only: the sync re-reads the configuration and re-evaluates every eligible board.
  const impl = syncImpl ?? syncMessage;
  const run = () => impl({
    client: ctx.client, guild: ctx.guild, channel, messageId: ctx.message.id, ensureMessage: ctx.ensureMessage,
  });
  const promise = coalescer.request(ctx.message.id, run);
  if (!promise) return 'coalesced';
  promise.catch(logSyncError);
  return 'queued';
}

/** Hook `run` for messageReactionAdd/Remove/RemoveAll/RemoveEmoji: returns at once, the work continues in the background. */
export function onReactionEvent(ctx) {
  handleReaction(ctx).catch(logSyncError);
}

// ── Hook: messageUpdate ───────────────────────────────────────────────────────

async function mirrorEdit(ctx) {
  const message = ctx.newMessage ?? ctx.message;
  const guild = ctx.guild;
  const boards = await getBoards(guild.id);
  if (!boards) return;
  const enabled = boards.filter((b) => b.enabled);
  if (!enabled.length) return;
  const entries = await prisma.starboardEntry.findMany({
    where: { sourceMessageId: message.id, boardMessageId: { not: null }, blocked: false, starboardId: { in: enabled.map((b) => b.id) } },
  });
  for (const entry of entries) {
    try {
      const board = enabled.find((b) => b.id === entry.starboardId);
      if (!board) continue;
      const qualifying = [...(message.reactions?.cache?.values() ?? [])].filter((r) => boardMatchesEmoji(board, emojiKeyOf(r.emoji)));
      const payload = buildRepostPayload({
        message, count: entry.count, emojiText: pickDisplayEmoji({ board, qualifying, client: ctx.client, message }),
      });
      const bc = await resolveBoardChannel(guild, board);
      if (!bc) continue;
      await editRepost({ guild, board, entry, bc, payload });
    } catch (err) {
      logSyncError(err);
    }
  }
}

/**
 * Hook `run` for messageUpdate (the dispatcher already dropped bot and webhook authors, so edits of starred bot messages
 * are not mirrored). Shares the keyed mutex and the semaphore with the reaction syncs, and is never coalesced.
 */
export function onMessageUpdate(ctx) {
  const message = ctx.newMessage ?? ctx.message;
  if (!message?.id) return;
  mutex.run(message.id, () => semaphore.run(() => mirrorEdit(ctx))).catch(logSyncError);
}

// ── Hooks: deletions ──────────────────────────────────────────────────────────

/** Deletes the reposts of one deleted source message and forgets its entries. Runs inside the per-message mutex. */
async function deleteSourceEntries({ client, guild, guildId, messageId }) {
  // The lookup happens inside the mutex, after any in-flight sync for this message has finished: a repost that was sent
  // but not yet recorded would otherwise outlive its deleted source.
  const entries = await prisma.starboardEntry.findMany({
    where: { sourceMessageId: messageId, starboard: { guildId } },
    include: { starboard: { select: { channelId: true, name: true } } },
  });
  for (const entry of entries) {
    if (entry.boardMessageId) {
      markBotDeletion(entry.boardMessageId, 'starboard');
      try {
        const bc = guild?.channels?.cache?.get(entry.starboard.channelId) ?? await client.channels.fetch(entry.starboard.channelId).catch(() => null);
        await bc?.messages.delete(entry.boardMessageId);
      } catch (err) {
        if (isNoAccess(err)) {
          logger.warn(`Starboard "${entry.starboard.name}": could not delete the repost of a deleted message; remove it by hand (${guildId})`);
        } else if (!isGone(err)) {
          logger.error('Starboard: could not delete the repost of a deleted message', err);
        }
      }
    }
    // The repost is deleted first, so a deleted message's content never lingers because of a crash between the two steps.
    await prisma.starboardEntry.deleteMany({ where: { id: entry.id } });
  }
}

/** Hook `run` for messageDelete (always). A staff-deleted repost is blocked; a deleted source loses its reposts. */
export function onMessageDelete(ctx) {
  (async () => {
    const id = ctx.message.id;
    if (wasDeletedByBot(id) === 'starboard') return;
    const boards = await getBoards(ctx.guildId);
    if (!boards || !boards.length) return;

    if (boards.some((b) => b.channelId === ctx.message.channelId)) {
      const entry = await prisma.starboardEntry.findFirst({
        where: { boardMessageId: id, starboard: { guildId: ctx.guildId } }, select: { id: true, sourceMessageId: true },
      });
      if (!entry) return;
      // The boardMessageId guard makes this a no-op when a concurrent sync already replaced or dropped the repost.
      await mutex.run(entry.sourceMessageId, () => prisma.starboardEntry.updateMany({
        where: { id: entry.id, boardMessageId: id }, data: { boardMessageId: null, blocked: true },
      }));
      return;
    }
    await mutex.run(id, () => deleteSourceEntries({ client: ctx.client, guild: ctx.guild, guildId: ctx.guildId, messageId: id }));
  })().catch(logSyncError);
}

/** Hook `run` for messageDeleteBulk (always): purged reposts are blocked, purged sources lose their reposts. */
export function onMessageDeleteBulk(ctx) {
  (async () => {
    const ids = [...ctx.messages.keys()];
    if (!ids.length) return;
    const boards = await getBoards(ctx.guildId);
    if (!boards || !boards.length) return;

    if (boards.some((b) => b.channelId === ctx.channel?.id)) {
      const repostIds = ids.filter((id) => wasDeletedByBot(id) !== 'starboard');
      if (repostIds.length) {
        await prisma.starboardEntry.updateMany({
          where: { boardMessageId: { in: repostIds }, starboard: { guildId: ctx.guildId } },
          data: { boardMessageId: null, blocked: true },
        });
      }
      return;
    }
    const sources = await prisma.starboardEntry.findMany({
      where: { sourceMessageId: { in: ids }, starboard: { guildId: ctx.guildId } }, select: { sourceMessageId: true }, distinct: ['sourceMessageId'],
    });
    await mapLimit(sources, 5, (s) => mutex.run(s.sourceMessageId, () => deleteSourceEntries({
      client: ctx.client, guild: ctx.guild, guildId: ctx.guildId, messageId: s.sourceMessageId,
    })).catch(logSyncError));
  })().catch(logSyncError);
}

/** Hook `run` for channelDelete (always, awaited, database only). A deleted board channel disables its boards. */
export async function onChannelDelete(ctx) {
  try {
    const guildId = ctx.guildId;
    const channel = ctx.channel;
    const boards = await getBoards(guildId, { fresh: true });
    if (!boards || !boards.length) return;
    let changed = false;

    const hit = boards.filter((b) => b.channelId === channel.id);
    if (hit.length) {
      const hitIds = hit.map((b) => b.id);
      await prisma.$transaction([
        prisma.starboard.updateMany({ where: { guildId, channelId: channel.id }, data: { enabled: false } }),
        prisma.starboardEntry.updateMany({ where: { starboardId: { in: hitIds } }, data: { boardMessageId: null } }),
      ]);
      changed = true;
      logger.info(`Starboard(s) ${hit.map((b) => b.name).join(', ')} disabled: channel deleted (${guildId})`);
    }
    if (boards.some((b) => b.ignoredChannelIds.includes(channel.id))) {
      await prisma.$executeRaw(removeIgnoredChannelSql(guildId, channel.id));
      changed = true;
    }
    // The reposts stay as an archive, but they can no longer update.
    const gone = await prisma.starboardEntry.deleteMany({ where: { sourceChannelId: channel.id, starboard: { guildId } } });
    if (gone.count > 0) changed = true;
    if (changed) await invalidateBoards(guildId);
  } catch (err) {
    logger.error('starboard channelDelete failed', err);
  }
}

// ── CRUD used by /starboard ───────────────────────────────────────────────────

const TX_OPTS = { maxWait: 10_000, timeout: 10_000 };

/**
 * Creates a board under the per-guild cap. Unspecified options in `data` are omitted so the schema defaults apply.
 * P2002 (duplicate name) is rethrown unchanged.
 */
export async function createBoard(guildId, data) {
  await ensureGuildSettings(guildId);
  const row = await prisma.$transaction(async (tx) => {
    // The row lock serialises concurrent creates against the cap.
    await tx.$queryRaw(lockGuildSettingsRow(guildId));
    if ((await tx.starboard.count({ where: { guildId } })) >= LIMITS.STARBOARDS) throw new BoardLimitError();
    return tx.starboard.create({ data: { guildId, ...Object.fromEntries(Object.entries(data).filter(([, v]) => v !== undefined)) } });
  }, TX_OPTS);
  await invalidateBoards(guildId);
  return row;
}

export function getBoardByName(guildId, name) {
  if (name == null) return Promise.resolve(null);
  return prisma.starboard.findUnique({ where: { guildId_name: { guildId, name } } });
}

const PATCH_FIELDS = [
  'name', 'channelId', 'emojis', 'threshold', 'selfStar', 'ignoreBots', 'removeBelowThreshold', 'autoReact',
  'maxMessageAgeDays', 'allowNsfw', 'removeSelfStars', 'enabled',
];

/**
 * Edits a board in one transaction. A channel change drops every live entry (they are reposted into the new channel).
 * @returns {Promise<{ before: object, after: object }>}
 */
export async function updateBoard(guildId, boardId, patch) {
  const result = await prisma.$transaction(async (tx) => {
    await tx.$queryRaw(lockBoardRow(boardId, guildId));
    const before = await tx.starboard.findFirst({ where: { id: boardId, guildId } });
    if (!before) throw new NotFoundError();
    const data = {};
    for (const key of PATCH_FIELDS) if (patch[key] !== undefined) data[key] = patch[key];
    if (patch.addIgnored || patch.removeIgnored) {
      let list = [...before.ignoredChannelIds];
      if (patch.addIgnored) list = [...new Set([...list, patch.addIgnored])];
      if (patch.removeIgnored) list = list.filter((id) => id !== patch.removeIgnored);
      if (list.length > MAX_IGNORED_CHANNELS) throw new IgnoreLimitError();
      data.ignoredChannelIds = list;
    }
    if ((data.selfStar ?? before.selfStar) && (data.removeSelfStars ?? before.removeSelfStars)) throw new SettingsConflictError();
    const after = await tx.starboard.update({ where: { id: boardId }, data });
    if (data.channelId && data.channelId !== before.channelId) {
      await tx.starboardEntry.updateMany({ where: { starboardId: boardId }, data: { boardMessageId: null } });
    }
    return { before, after };
  }, TX_OPTS);
  await invalidateBoards(guildId);
  return result;
}

/** @returns {Promise<number>} rows deleted (their entries cascade) */
export async function deleteBoard(guildId, boardId) {
  const { count } = await prisma.starboard.deleteMany({ where: { id: boardId, guildId } });
  await invalidateBoards(guildId);
  return count;
}

/** Every board of the guild by creation, each with `reposts` (entries that have a live repost). */
export async function listBoardsWithCounts(guildId) {
  const boards = await prisma.starboard.findMany({ where: { guildId }, orderBy: { createdAt: 'asc' } });
  if (!boards.length) return [];
  const groups = await prisma.starboardEntry.groupBy({
    by: ['starboardId'],
    where: { starboardId: { in: boards.map((b) => b.id) }, boardMessageId: { not: null } },
    _count: { _all: true },
  });
  const counts = new Map(groups.map((g) => [g.starboardId, g._count._all]));
  return boards.map((b) => ({ ...b, reposts: counts.get(b.id) ?? 0 }));
}

/** Autocomplete lookup, filtered in the database. Never throws. */
export async function searchBoards(guildId, focused) {
  try {
    return await prisma.starboard.findMany({
      where: { guildId, name: { contains: String(focused ?? '').toLowerCase().slice(0, 32), mode: 'insensitive' } },
      orderBy: { name: 'asc' }, take: 25, select: { name: true, channelId: true, enabled: true },
    });
  } catch (err) {
    logger.debug(`starboard autocomplete failed: ${err?.message}`);
    return [];
  }
}

/** The settings block shared by create, edit and list. `reposts` is shown only when known (list). */
export function settingsSummary(board, { client, reposts } = {}) {
  const self = board.selfStar ? 'on' : board.removeSelfStars ? 'removed' : 'off';
  const last = [
    `Max age: ${board.maxMessageAgeDays > 0 ? `${board.maxMessageAgeDays} days` : 'no limit'}`,
    `NSFW: ${board.allowNsfw ? 'allowed' : 'blocked'}`,
  ];
  if (reposts !== undefined) last.push(`Reposts: ${reposts}`);
  return [
    `Channel: <#${board.channelId}> · Emojis: ${formatEmojiList(board.emojis, client)}`,
    `Threshold: ${board.threshold} · Self-star: ${self} · Bots: ${board.ignoreBots ? 'ignored' : 'counted'}`,
    `Below threshold: ${board.removeBelowThreshold ? 'remove' : 'keep'} · Auto-react: ${board.autoReact ? 'on' : 'off'}`,
    last.join(' · '),
  ].join('\n');
}
