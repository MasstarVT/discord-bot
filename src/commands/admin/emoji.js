// /emoji add | list | delete (pkg-emojis.md). Core ADMIN command (module = null): copies a custom emoji or uploads an image link as a
// server emoji, lists the server's emojis with the slot usage for its boost tier, and deletes one after a confirm dialog.
// There is no database or Redis access. Discord is the source of truth (guild.emojis.cache); the only shared state is the
// in-process per-guild upload guard, because Discord rate-limits emoji creation per guild and discord.js waits on a 429
// instead of failing. Every image download goes through safeFetchImage (SSRF-safe, 256 KiB cap, content type and dimension
// checks); uploads are always our own `data:<sniffed mime>;base64,` string, never a URL or a bare Buffer.
import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { successEmbed, errorEmbed, warnEmbed, guildEmbed } from '../../utils/embedBuilder.js';
import { safeFetchImage, SafeFetchError } from '../../utils/safeFetch.js';
import {
  safeDefer, safeReply, replyError, missingBotPermissions, respondAutocomplete, awaitConfirm,
} from '../../utils/interactionHelpers.js';
import { paginate } from '../../utils/paginator.js';
import { clampText } from '../../utils/text.js';
import { withTimeout, TimeoutError } from '../../utils/async.js';
import logger from '../../utils/logger.js';

export const module = null;                                // core command: always available, never toggled
export const permissionLevel = PERMISSION_LEVELS.ADMIN;   // every subcommand is ADMIN; guild-only (no allowInDMs)

export const data = new SlashCommandBuilder()
  .setName('emoji')
  .setDescription('Add, list or delete emojis (MEE6 /add_emoji)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild | PermissionFlagsBits.ManageGuildExpressions)
  .addSubcommand((s) => s.setName('add')
    .setDescription('Copy an emoji or upload an image URL')
    .addStringOption((o) => o.setName('emoji')
      .setDescription('Custom emoji or image URL (max 256 KB)')
      .setRequired(true).setMaxLength(512))
    .addStringOption((o) => o.setName('name')
      .setDescription('2–32 letters, numbers or _')
      .setMinLength(2).setMaxLength(32))
    .addRoleOption((o) => o.setName('role')
      .setDescription('Only this role can use it')))
  .addSubcommand((s) => s.setName('list')
    .setDescription('List emojis and free slots'))
  .addSubcommand((s) => s.setName('delete')
    .setDescription('Delete an emoji (with confirm)')
    .addStringOption((o) => o.setName('emoji')
      .setDescription('Emoji, name or ID')
      .setRequired(true).setMaxLength(100).setAutocomplete(true)));

// ── Constants ─────────────────────────────────────────────────────────────────

export const EMOJI_MAX_BYTES = 256 * 1024;            // 262_144, Discord's emoji upload limit
export const EMOJI_NAME_RE = /^[A-Za-z0-9_]{2,32}$/;
export const TIER_EMOJI_LIMITS = Object.freeze([50, 100, 150, 250]);   // index = premiumTier
export const MORE_EMOJI_LIMIT = 200;                  // guild feature 'MORE_EMOJI' (string literal; not in discord.js GuildFeature)
export const LIST_PAGE_SIZE = 20;
export const DOWNLOAD_TIMEOUT_MS = 8_000;
export const CREATE_TIMEOUT_MS = 30_000;
export const DELETE_TIMEOUT_MS = 15_000;
export const IN_FLIGHT_STALE_MS = 15 * 60_000;
const DISCORD_CDN_HOSTS = new Set(['cdn.discordapp.com', 'media.discordapp.net']);
const NO_MENTIONS = Object.freeze({ parse: [] });

// ── Pure helpers (no Discord calls, DB or Redis) ──────────────────────────────

/** An array, or anything with values() (Map, Collection), as an array. */
const toList = (x) => (Array.isArray(x) ? x : [...(x?.values?.() ?? [])]);

/**
 * What the `emoji` option of /emoji add points at. Never throws.
 * @param {unknown} input
 * @returns {{ kind: 'custom', id: string, name: string|null, animated: boolean }
 *   | { kind: 'url', url: string, nameHint: string|null }
 *   | { kind: 'shortcode', name: string } | { kind: 'id', id: string }
 *   | { kind: 'unicode' } | { kind: 'empty' } | { kind: 'invalid' }}
 */
export function parseEmojiSource(input) {
  let s = String(input ?? '').trim();
  if (!s) return { kind: 'empty' };

  // Discord's embed-suppression brackets around a link.
  const wrapped = /^<(https?:\/\/[^\s<>]+)>$/i.exec(s);
  if (wrapped) s = wrapped[1];

  const mention = /^<(a?):(\w{1,32}):(\d{17,21})>$/.exec(s);
  if (mention) return { kind: 'custom', id: mention[3], name: mention[2], animated: mention[1] === 'a' };

  if (/^https?:\/\//i.test(s)) {
    let u;
    try {
      u = new URL(s);
    } catch {
      return { kind: 'invalid' };
    }
    if (DISCORD_CDN_HOSTS.has(u.hostname)) {
      const m = /^\/emojis\/(\d{17,21})(?:\.(png|gif|webp|jpe?g|avif))?$/i.exec(u.pathname);
      if (m) {
        const n = sanitizeEmojiName(u.searchParams.get('name') ?? '');
        return {
          kind: 'custom',
          id: m[1],
          name: EMOJI_NAME_RE.test(n) ? n : null,
          animated: (m[2] ?? '').toLowerCase() === 'gif' || u.searchParams.get('animated') === 'true',
        };
      }
    }
    return { kind: 'url', url: u.href, nameHint: nameFromUrl(u.href) };
  }

  // Before the shortcode check: digits also match the shortcode pattern.
  if (/^\d{17,21}$/.test(s)) return { kind: 'id', id: s };

  // Both colons: Discord sends ":name:" for an emoji the sender cannot use. A bare word is just text.
  const short = /^:([A-Za-z0-9_]{2,32})(?:~\d+)?:$/.exec(s);
  if (short) return { kind: 'shortcode', name: short[1] };

  if (/\p{Extended_Pictographic}|\p{Regional_Indicator}/u.test(s)) return { kind: 'unicode' };
  return { kind: 'invalid' };
}

/**
 * The Discord CDN image of a custom emoji.
 * @param {string} id
 * @param {boolean} animated
 * @param {{ size?: number|null }} [opts]
 * @returns {string}
 */
export function emojiCdnUrl(id, animated, { size = null } = {}) {
  return `https://cdn.discordapp.com/emojis/${id}.${animated ? 'gif' : 'png'}${size ? `?size=${size}` : ''}`;
}

/**
 * Cleans a name toward [A-Za-z0-9_]. The result may be '' or one character long; the caller validates it.
 * @param {unknown} raw
 * @returns {string}
 */
export function sanitizeEmojiName(raw) {
  return String(raw ?? '')
    .normalize('NFKD').replace(/[̀-ͯ]/g, '')   // é becomes e
    .trim()
    .replace(/[\s.\-]+/g, '_')
    .replace(/[^A-Za-z0-9_]/g, '')
    .slice(0, 32);
}

/**
 * A usable emoji name from a link: its ?name= parameter, or the file name without extension.
 * @param {string} url
 * @returns {string|null}
 */
export function nameFromUrl(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  const fromQuery = sanitizeEmojiName(u.searchParams.get('name') ?? '');
  if (EMOJI_NAME_RE.test(fromQuery)) return fromQuery;
  const segment = u.pathname.split('/').filter(Boolean).pop();
  if (!segment) return null;
  let decoded = segment;
  try {
    decoded = decodeURIComponent(segment);
  } catch {
    // keep the raw segment
  }
  const n = sanitizeEmojiName(decoded.replace(/\.[A-Za-z0-9]{2,5}$/, ''));
  return EMOJI_NAME_RE.test(n) && !/^\d+$/.test(n) ? n : null;
}

/**
 * The name for a new emoji: the given one (sanitised, must be valid) or one derived from the source.
 * @param {{ given?: string|null, source?: object }} args
 * @returns {{ ok: true, name: string, adjusted: boolean, derived: boolean } | { ok: false, reason: string }}
 */
export function resolveEmojiName({ given, source } = {}) {
  if (given != null) {
    const raw = String(given);
    const name = sanitizeEmojiName(raw);
    if (!EMOJI_NAME_RE.test(name)) {
      return { ok: false, reason: 'Emoji names need 2–32 characters: letters, numbers or underscores.' };
    }
    return { ok: true, name, adjusted: name !== raw.trim(), derived: false };
  }
  const base = source?.kind === 'custom' ? source.name : source?.kind === 'url' ? source.nameHint : null;
  let name = sanitizeEmojiName(base ?? '');
  if (!EMOJI_NAME_RE.test(name) || /^\d+$/.test(name)) name = 'emoji';
  return { ok: true, name, adjusted: false, derived: true };
}

/**
 * The lowercased names of every emoji, managed ones included.
 * @param {Array|Map|import('discord.js').Collection} emojis
 * @returns {Set<string>}
 */
export function takenNames(emojis) {
  return new Set(toList(emojis).map((e) => String(e?.name ?? '').toLowerCase()));
}

/**
 * `base`, or `base_2` … `base_99` (cut so the result stays within 32 characters), whichever is free first.
 * @param {string} base
 * @param {Set<string>} takenLower
 * @returns {string|null} null when all are taken
 */
export function pickFreeName(base, takenLower) {
  if (!takenLower.has(base.toLowerCase())) return base;
  for (let n = 2; n <= 99; n++) {
    const suffix = `_${n}`;
    const cand = base.slice(0, 32 - suffix.length) + suffix;
    if (!takenLower.has(cand.toLowerCase())) return cand;
  }
  return null;
}

/**
 * Emoji slots per kind (static and animated are counted separately) for a boost tier.
 * @param {number} [premiumTier]
 * @param {string[]} [features]
 * @returns {number}
 */
export function emojiSlotLimit(premiumTier, features = []) {
  const tier = TIER_EMOJI_LIMITS[premiumTier] ?? TIER_EMOJI_LIMITS[0];
  return Array.isArray(features) && features.includes('MORE_EMOJI') ? Math.max(MORE_EMOJI_LIMIT, tier) : tier;
}

/**
 * Slot usage. Managed (integration) emojis are not counted; unavailable ones are. Pass the Guild as the second argument.
 * @param {Array|Map|import('discord.js').Collection} emojis
 * @param {{ premiumTier?: number, features?: string[] }} [guild]
 * @returns {{ tier: number, static: { used: number, max: number, free: number }, animated: { used: number, max: number, free: number } }}
 */
export function countEmojiSlots(emojis, { premiumTier = 0, features = [] } = {}) {
  const max = emojiSlotLimit(premiumTier, features);
  let staticUsed = 0;
  let animatedUsed = 0;
  for (const e of toList(emojis)) {
    if (e?.managed) continue;
    if (e?.animated) animatedUsed++;
    else staticUsed++;
  }
  return {
    tier: premiumTier ?? 0,
    static: { used: staticUsed, max, free: Math.max(0, max - staticUsed) },
    animated: { used: animatedUsed, max, free: Math.max(0, max - animatedUsed) },
  };
}

/** @param {ReturnType<typeof countEmojiSlots>} slots @returns {string} */
export function formatSlots(slots) {
  return `Static ${slots.static.used}/${slots.static.max} · Animated ${slots.animated.used}/${slots.animated.max} · Boost level ${slots.tier}`;
}

/**
 * The image type from its magic bytes (the Content-Type header is never trusted). SVG, AVIF and everything else is null.
 * @param {unknown} buffer
 * @returns {{ mime: string, ext: string, animated: boolean }|null}
 */
export function sniffImage(buffer) {
  if (!Buffer.isBuffer(buffer) || buffer.length < 12) return null;
  const ascii = (from, to) => buffer.toString('latin1', from, to);
  if (buffer[0] === 0x89 && ascii(1, 4) === 'PNG' && buffer[4] === 0x0d && buffer[5] === 0x0a && buffer[6] === 0x1a && buffer[7] === 0x0a) {
    return { mime: 'image/png', ext: 'png', animated: false };   // APNG counts as static
  }
  if (buffer[0] === 0xff && buffer[1] === 0xd8 && buffer[2] === 0xff) return { mime: 'image/jpeg', ext: 'jpg', animated: false };
  const head = ascii(0, 6);
  if (head === 'GIF87a' || head === 'GIF89a') return { mime: 'image/gif', ext: 'gif', animated: true };
  if (ascii(0, 4) === 'RIFF' && ascii(8, 12) === 'WEBP') {
    const animated = ascii(12, 16) === 'VP8X' && buffer.length > 20 && (buffer[20] & 0x02) !== 0;
    return { mime: 'image/webp', ext: 'webp', animated };
  }
  return null;
}

/**
 * A human text for a failed download.
 * @param {unknown} err
 * @param {{ fromCdn?: boolean }} [opts]
 * @returns {string}
 */
export function describeFetchError(err, { fromCdn = false } = {}) {
  if (!(err instanceof SafeFetchError)) return 'The image could not be downloaded.';
  switch (err.code) {
    case 'INVALID_URL': return err.message;
    case 'BLOCKED_ADDRESS': return 'That link points to a private or reserved network address.';
    case 'DNS_FAILED': return "I couldn't resolve that link's host name.";
    case 'TIMEOUT': return 'The image took too long to download (8 s limit).';
    case 'TOO_LARGE': return "That image is larger than 256 KB, Discord's limit for emojis. Use a smaller image.";
    case 'TOO_MANY_REDIRECTS': return 'That link redirects too many times.';
    case 'HTTP_ERROR':
      return fromCdn && err.status === 404
        ? "Discord's CDN doesn't have that emoji (it may have been deleted)."
        : `The link returned HTTP ${err.status ?? '?'}.`;
    case 'BAD_CONTENT_TYPE': return 'That link is not a PNG, JPG, GIF or WebP image.';
    case 'BAD_IMAGE': return 'That file is not a valid image (it may be corrupt or have too large dimensions).';
    case 'NETWORK': return 'A network error stopped the download. Try again in a moment.';
    default: return 'The image could not be downloaded.';
  }
}

/**
 * A human text for a failed create or delete (a Discord REST error or a TimeoutError).
 * @param {unknown} err
 * @param {{ action?: 'add'|'delete' }} [opts]
 * @returns {string}
 */
export function describeEmojiError(err, { action = 'add' } = {}) {
  if (err instanceof TimeoutError) {
    return action === 'delete'
      ? 'Discord is rate-limiting emoji changes right now. The deletion is queued and will finish on its own; check `/emoji list` later.'
      : "Discord is rate-limiting emoji uploads for this server. Your upload is queued and will finish on its own when Discord allows it (this can take a while). Check `/emoji list` later before trying again.";
  }
  switch (Number(err?.code)) {
    case 30008: return 'This server has no free static emoji slots.';
    case 30018: return 'This server has no free animated emoji slots.';
    case 50013:
      return action === 'delete'
        ? 'I need the **Manage Expressions** permission to delete that emoji (with only Create Expressions I can delete just the emojis I uploaded).'
        : 'I need the **Manage Expressions** (or **Create Expressions**) permission to upload emojis.';
    case 50001: return "I can't access this server's emojis.";
    case 50045: return 'That image is larger than 256 KB.';
    case 50046: return "Discord couldn't read that image file.";
    case 50138: return "Discord couldn't resize that image to fit its emoji limits. Try a smaller or simpler image.";
    case 50035: return `Discord rejected the emoji: ${clampText(String(err?.message ?? ''), 300)}`;
    case 10014: return 'That emoji no longer exists.';
    default: return `Discord returned an error: ${clampText(String(err?.message ?? 'unknown error'), 200)}`;
  }
}

// Codes that are expected user situations: logged at debug, not warn.
const EXPECTED_DISCORD_CODES = new Set([30008, 30018, 50013, 50045, 50046, 50138, 10014]);

/** One line of /emoji list. @param {object} e @returns {string} */
export function formatEmojiLine(e) {
  const tags = [];
  if (e.animated) tags.push('animated');
  const rc = e.roles?.cache?.size ?? 0;
  if (rc) tags.push(`🔒 ${rc} role${rc === 1 ? '' : 's'}`);
  if (e.managed) tags.push('managed');
  if (e.available === false) tags.push('⚠️ unavailable');
  return `<${e.animated ? 'a' : ''}:${e.name}:${e.id}> \`:${e.name}:\` · \`${e.id}\`${tags.length ? ` · ${tags.join(' · ')}` : ''}`;
}

/**
 * The /emoji list pages: static before animated, each by name then id.
 * @param {Array|Map|import('discord.js').Collection} emojis
 * @param {ReturnType<typeof countEmojiSlots>} slots
 * @param {{ perPage?: number }} [opts]
 * @returns {Array<{ description: string, footer: string }>} [] for no emojis
 */
export function buildListPages(emojis, slots, { perPage = LIST_PAGE_SIZE } = {}) {
  const list = toList(emojis);
  if (list.length === 0) return [];
  const size = Math.min(25, Math.max(1, Math.floor(Number(perPage)) || LIST_PAGE_SIZE));
  const sorted = [...list].sort((a, b) => {
    if (Boolean(a.animated) !== Boolean(b.animated)) return a.animated ? 1 : -1;
    const an = String(a.name ?? '').toLowerCase();
    const bn = String(b.name ?? '').toLowerCase();
    if (an !== bn) return an < bn ? -1 : 1;
    return String(a.id) < String(b.id) ? -1 : String(a.id) > String(b.id) ? 1 : 0;
  });
  const total = sorted.length;
  const footer = `${total} custom emoji${total === 1 ? '' : 's'}`;
  const pages = [];
  for (let i = 0; i < total; i += size) {
    const lines = sorted.slice(i, i + size).map(formatEmojiLine);
    pages.push({ description: clampText(`${formatSlots(slots)}\n\n${lines.join('\n')}`, 4096), footer });
  }
  return pages;
}

/**
 * Finds one of the server's emojis from a mention, an ID, or a name (exact case first, then case-insensitive).
 * @param {Array|Map|import('discord.js').Collection} emojis
 * @param {unknown} input
 * @returns {{ ok: true, emoji: object } | { ok: false, reason: string }}
 */
export function resolveGuildEmoji(emojis, input) {
  const list = toList(emojis);
  const s = String(input ?? '').trim();
  if (!s) return { ok: false, reason: 'Pick an emoji from the list, or paste it or its ID.' };

  const mention = /^<a?:\w{1,32}:(\d{17,21})>$/.exec(s);
  if (mention) {
    const found = list.find((e) => e.id === mention[1]);
    return found ? { ok: true, emoji: found } : { ok: false, reason: "That emoji isn't from this server." };
  }
  if (/^\d{17,21}$/.test(s)) {
    const found = list.find((e) => e.id === s);
    return found ? { ok: true, emoji: found } : { ok: false, reason: 'No emoji with that ID exists in this server.' };
  }

  const name = s.replace(/^:|:$/g, '');
  const ambiguous = { ok: false, reason: `Several emojis are named \`:${clampText(name, 40)}:\`. Pick one from the list or use its ID.` };
  const exact = list.filter((e) => e.name === name);
  if (exact.length === 1) return { ok: true, emoji: exact[0] };
  if (exact.length > 1) return ambiguous;
  const lower = name.toLowerCase();
  const loose = list.filter((e) => String(e.name ?? '').toLowerCase() === lower);
  if (loose.length === 1) return { ok: true, emoji: loose[0] };
  if (loose.length > 1) return ambiguous;
  return { ok: false, reason: `No emoji named \`:${clampText(name, 40)}:\` in this server.` };
}

/**
 * Autocomplete choices for /emoji delete: at most 25 non-managed emojis, values are IDs.
 * @param {Array|Map|import('discord.js').Collection} emojis
 * @param {unknown} query
 * @returns {Array<{ name: string, value: string }>}
 */
export function emojiChoices(emojis, query) {
  const q = String(query ?? '').trim().toLowerCase();
  const idMatch = /(\d{17,21})/.exec(q)?.[1];
  const pool = toList(emojis).filter((e) => !e?.managed);
  let picked;
  if (idMatch) {
    picked = pool.filter((e) => e.id === idMatch);
  } else {
    const nq = q.replace(/[^a-z0-9_]/g, '');
    picked = pool
      .filter((e) => String(e.name ?? '').toLowerCase().includes(nq))
      .sort((a, b) => {
        const an = String(a.name ?? '').toLowerCase();
        const bn = String(b.name ?? '').toLowerCase();
        const as = an.startsWith(nq) ? 0 : 1;
        const bs = bn.startsWith(nq) ? 0 : 1;
        if (as !== bs) return as - bs;
        return an < bn ? -1 : an > bn ? 1 : 0;
      });
  }
  return picked.slice(0, 25).map((e) => ({ name: `:${e.name}:${e.animated ? ' (animated)' : ''}`, value: e.id }));
}

// ── Per-guild upload guard (in-process; a guild's commands always reach its own shard) ─────

const uploadsInFlight = new Map();   // guildId → { token, startedAt }

/**
 * Claims the guild's upload slot. Discord rate-limits emoji creation per guild and discord.js waits on a 429, so uploads
 * must not stack up behind one another.
 * @param {string} guildId
 * @param {number} [now]
 * @returns {symbol|null} a token for endEmojiUpload, or null when an upload is already running (and not stale)
 */
export function beginEmojiUpload(guildId, now = Date.now()) {
  const cur = uploadsInFlight.get(guildId);
  if (cur && now - cur.startedAt < IN_FLIGHT_STALE_MS) return null;
  const token = Symbol('emoji-upload');
  uploadsInFlight.set(guildId, { token, startedAt: now });
  return token;
}

/** Releases the slot only when `token` still owns it, so a stale upload can never clear a newer one's entry. */
export function endEmojiUpload(guildId, token) {
  if (uploadsInFlight.get(guildId)?.token === token) uploadsInFlight.delete(guildId);
}

// ── Download ──────────────────────────────────────────────────────────────────

async function downloadEmojiImage(source) {
  const opts = { maxBytes: EMOJI_MAX_BYTES, timeoutMs: DOWNLOAD_TIMEOUT_MS };
  if (source.kind === 'custom') {
    try {
      return await safeFetchImage(emojiCdnUrl(source.id, source.animated), opts);
    } catch (err) {
      if (err instanceof SafeFetchError && err.code === 'TOO_LARGE') {
        return await safeFetchImage(emojiCdnUrl(source.id, source.animated, { size: 96 }), opts);   // one retry, smaller
      }
      throw err;
    }
  }
  return await safeFetchImage(source.url, opts);   // kind 'url'
}

// Test seam: replaces the download and the create timeout. Called with no arguments it restores the defaults.
let downloadImpl = downloadEmojiImage;
let createTimeoutMs = CREATE_TIMEOUT_MS;
export function __setTestHooks({ download, createTimeout } = {}) {
  downloadImpl = download ?? downloadEmojiImage;
  createTimeoutMs = createTimeout ?? CREATE_TIMEOUT_MS;
}

// ── Command helpers ───────────────────────────────────────────────────────────

const botHas = (guild, key) => missingBotPermissions(guild, [key]).length === 0;
const botCanCreate = (guild) => botHas(guild, 'ManageGuildExpressions') || botHas(guild, 'CreateGuildExpressions');
const botCanManage = (guild) => botHas(guild, 'ManageGuildExpressions');

const auditReason = (interaction, sub) => clampText(`${interaction.user.tag} (${interaction.user.id}) via /emoji ${sub}`, 512);

// Guild-level permissions of the invoker. Administrators and the owner pass every has().
function invokerPermissions(interaction) {
  const fromMember = interaction.member?.permissions;
  return typeof fromMember?.has === 'function' ? fromMember : interaction.memberPermissions;
}

const noSlotsText = (kind, slots) => `This server has no free ${kind} emoji slots (${slots[kind].used}/${slots[kind].max}, boost level ${slots.tier}). `
  + 'Delete one with `/emoji delete` or boost the server.';

function logDiscordError(sub, err) {
  if (EXPECTED_DISCORD_CODES.has(Number(err?.code))) logger.debug(`[emoji] ${sub} failed: ${err?.code} ${err?.message}`);
  else logger.warn(`[emoji] ${sub} failed`, err);
}

// ── /emoji add ────────────────────────────────────────────────────────────────

async function runAdd(interaction) {
  const guild = interaction.guild;

  // Pre-defer checks use only synchronous data, so they stay inside the 3-second window. Failures are ephemeral.
  const perms = invokerPermissions(interaction);
  if (!perms?.has(PermissionFlagsBits.ManageGuildExpressions)) {
    return replyError(interaction, 'Missing Permissions', 'You need the **Manage Expressions** permission to add emojis.');
  }
  if (!botCanCreate(guild)) {
    return replyError(interaction, 'Missing Permissions', 'I need the **Manage Expressions** (or **Create Expressions**) permission in this server to add emojis.');
  }

  const source = parseEmojiSource(interaction.options.getString('emoji', true));
  switch (source.kind) {
    case 'empty':
    case 'invalid':
      return replyError(interaction, 'Invalid Emoji', "That isn't a custom emoji or an image link. Paste a custom emoji (`<:name:id>`) or a direct image link starting with https://. Tip: right-click an emoji → **Copy Link**.");
    case 'unicode':
      return replyError(interaction, 'Invalid Emoji', "Standard emojis are built into Discord and can't be uploaded. Paste a custom emoji or an image link instead.");
    case 'shortcode':
      return replyError(interaction, 'Invalid Emoji', `I only received the name \`:${source.name}:\`. Discord sends the full emoji only if you can use it (e.g. with Nitro). Right-click the emoji → **Copy Link** and paste that link instead.`);
    case 'id':
      return replyError(interaction, 'Invalid Emoji', "An ID alone doesn't tell me which image to copy or whether it is animated. Paste the emoji itself or its image link.");
    default:
      break;
  }

  if (source.kind === 'custom' && guild.emojis.cache.has(source.id)) {
    const existing = guild.emojis.cache.get(source.id);
    return replyError(interaction, 'Already Added', `That emoji is already in this server: ${existing} \`:${existing.name}:\``);
  }

  const role = interaction.options.getRole('role');
  if (role && role.id === guild.id) {
    return replyError(interaction, 'Invalid Role', 'Pick a specific role. Restricting an emoji to @everyone does nothing.');
  }

  const nameResult = resolveEmojiName({ given: interaction.options.getString('name'), source });
  if (!nameResult.ok) return replyError(interaction, 'Invalid Name', nameResult.reason);

  const taken = takenNames(guild.emojis.cache);
  let name = nameResult.name;
  let renamedFrom = null;
  if (nameResult.derived) {
    const free = pickFreeName(name, taken);
    if (free === null) {
      return replyError(interaction, 'Name Taken', `\`:${name}:\` and its numbered variants are taken. Choose a \`name\`.`);
    }
    if (free !== name) renamedFrom = name;
    name = free;
  } else if (taken.has(name.toLowerCase())) {
    return replyError(interaction, 'Name Taken', `An emoji named \`:${name}:\` already exists. Choose another \`name\`.`);
  }

  let slots = countEmojiSlots(guild.emojis.cache, guild);
  if (source.kind === 'custom') {
    const kind = source.animated ? 'animated' : 'static';
    if (slots[kind].free === 0) return replyError(interaction, 'No Free Slots', noSlotsText(kind, slots));
  } else if (slots.static.free === 0 && slots.animated.free === 0) {
    return replyError(interaction, 'No Free Slots', `This server has no free emoji slots (${formatSlots(slots)}).`);
  }

  const token = beginEmojiUpload(guild.id);
  if (token === null) {
    return replyError(interaction, 'Upload In Progress', "Another emoji upload is still running in this server (it may be waiting on Discord's rate limit). Try again in a moment.");
  }
  let handedOff = false;   // once the create promise owns the guard, the finally below must not release it
  try {
    if (!(await safeDefer(interaction))) return null;

    let buffer;
    try {
      buffer = await downloadImpl(source);
    } catch (err) {
      if (err instanceof SafeFetchError) logger.debug(`[emoji] download failed (${err.code}): ${err.message}`);
      else logger.warn('[emoji] download failed', err);
      return await safeReply(interaction, {
        embeds: [errorEmbed('Download Failed', describeFetchError(err, { fromCdn: source.kind === 'custom' }))],
        allowedMentions: NO_MENTIONS,
      });
    }

    const info = sniffImage(buffer);
    if (!info) {
      return await safeReply(interaction, {
        embeds: [errorEmbed('Unsupported Image', 'That file is not a PNG, JPG, GIF or WebP image.')],
        allowedMentions: NO_MENTIONS,
      });
    }

    // The download took a while: another admin may have filled the last slot. The image decides the kind now.
    slots = countEmojiSlots(guild.emojis.cache, guild);
    const kind = info.animated ? 'animated' : 'static';
    if (slots[kind].free === 0) {
      return await safeReply(interaction, {
        embeds: [errorEmbed('No Free Slots', noSlotsText(kind, slots))],
        allowedMentions: NO_MENTIONS,
      });
    }

    const createP = guild.emojis.create({
      attachment: `data:${info.mime};base64,${buffer.toString('base64')}`,
      name,
      roles: role ? [role.id] : undefined,
      reason: auditReason(interaction, 'add'),
    });
    // Hand the guard to the create promise. The two-callback form keeps this derived promise from ever rejecting.
    createP.then(() => endEmojiUpload(guild.id, token), () => endEmojiUpload(guild.id, token));
    handedOff = true;
    let emoji;
    try {
      emoji = await withTimeout(createP, createTimeoutMs, 'emoji create');
    } catch (err) {
      if (err instanceof TimeoutError) {
        return await safeReply(interaction, {
          embeds: [warnEmbed('Upload Queued', describeEmojiError(err, { action: 'add' }))],
          allowedMentions: NO_MENTIONS,
        });
      }
      logDiscordError('add', err);
      return await safeReply(interaction, {
        embeds: [errorEmbed('Upload Failed', describeEmojiError(err, { action: 'add' }))],
        allowedMentions: NO_MENTIONS,
      });
    }

    logger.info(`[emoji] ${guild.id}: added ${emoji.id} :${emoji.name}: by ${interaction.user.id}`);
    const after = countEmojiSlots(guild.emojis.cache, guild);   // the create action has already added the emoji to the cache
    const lines = [`${emoji} \`:${emoji.name}:\` is ready to use.`];
    if (role) lines.push(`Only members with <@&${role.id}> can use it.`);
    if (nameResult.adjusted) lines.push(`The name was adjusted to \`${emoji.name}\` (letters, numbers and _ only).`);
    if (renamedFrom) lines.push(`\`${renamedFrom}\` was taken, so it was named \`${emoji.name}\`.`);
    lines.push('', `**Slots:** ${formatSlots(after)}`);
    const embed = successEmbed('Emoji Added', lines.join('\n'));
    try {
      embed.setThumbnail(emoji.imageURL({ extension: emoji.animated ? 'gif' : 'png', size: 128 }));
    } catch (err) {
      logger.debug(`[emoji] thumbnail failed: ${err?.message}`);
    }
    return await safeReply(interaction, { embeds: [embed], allowedMentions: NO_MENTIONS });
  } finally {
    if (!handedOff) endEmojiUpload(guild.id, token);
  }
}

// ── /emoji list ───────────────────────────────────────────────────────────────

async function runList(interaction) {
  const guild = interaction.guild;
  if (!(await safeDefer(interaction))) return null;

  const settings = await getSettingsSafe(interaction.guildId);
  let emojis;
  try {
    emojis = await withTimeout(guild.emojis.fetch(), 10_000, 'emoji list fetch');
  } catch (err) {
    logger.debug(`[emoji] list fetch failed, using the cache: ${err?.message}`);
    emojis = guild.emojis.cache;
  }

  const slots = countEmojiSlots(emojis, guild);
  const pages = buildListPages(emojis, slots);
  if (pages.length === 0) {
    return safeReply(interaction, {
      embeds: [guildEmbed(
        settings ?? {},
        'Server Emojis',
        `This server has no custom emojis yet. Add one with \`/emoji add\`.\n\n**Slots:** ${formatSlots(slots)}`,
      )],
      allowedMentions: NO_MENTIONS,
    });
  }
  const embeds = pages.map((p) => guildEmbed(settings ?? {}, clampText(`Emojis in ${guild.name}`, 256), p.description, p.footer));
  return paginate(interaction, embeds);
}

// ── /emoji delete ─────────────────────────────────────────────────────────────

async function runDelete(interaction) {
  const guild = interaction.guild;

  const perms = invokerPermissions(interaction);
  if (!perms?.has(PermissionFlagsBits.ManageGuildExpressions)) {
    return replyError(interaction, 'Missing Permissions', 'You need the **Manage Expressions** permission to delete emojis.');
  }
  // With only Create Expressions the attempt is still made: Discord lets the bot delete emojis it uploaded itself (50013 is mapped).
  if (!botCanManage(guild) && !botCanCreate(guild)) {
    return replyError(interaction, 'Missing Permissions', 'I need the **Manage Expressions** permission in this server to delete emojis.');
  }

  const r = resolveGuildEmoji(guild.emojis.cache, interaction.options.getString('emoji', true));
  if (!r.ok) return replyError(interaction, 'Emoji Not Found', r.reason);
  if (r.emoji.managed) {
    return replyError(interaction, 'Managed Emoji', "That emoji is managed by an integration (for example Twitch) and can't be deleted here.");
  }

  const { emoji } = r;
  const id = emoji.id;
  const name = emoji.name;
  const ok = await awaitConfirm(interaction, {
    title: 'Delete this emoji?',
    description: `${emoji} \`:${name}:\` will be removed for everyone. This cannot be undone.`,
    confirmLabel: 'Delete', cancelLabel: 'Cancel', danger: true, timeoutMs: 30_000, ephemeral: true,
  });
  if (!ok) return null;   // the confirm message already says "Cancelled" or "Timed out"

  if (!guild.emojis.cache.has(id)) {
    return safeReply(interaction, { embeds: [errorEmbed('Already Deleted', 'That emoji was already deleted.')], ephemeral: true });
  }

  try {
    await withTimeout(guild.emojis.delete(id, auditReason(interaction, 'delete')), DELETE_TIMEOUT_MS, 'emoji delete');
  } catch (err) {
    if (err instanceof TimeoutError) {
      return safeReply(interaction, { embeds: [warnEmbed('Deletion Queued', describeEmojiError(err, { action: 'delete' }))], ephemeral: true });
    }
    logDiscordError('delete', err);
    return safeReply(interaction, { embeds: [errorEmbed('Delete Failed', describeEmojiError(err, { action: 'delete' }))], ephemeral: true });
  }

  // The cache is updated only by the gateway event, so exclude the deleted id explicitly.
  const after = countEmojiSlots([...guild.emojis.cache.values()].filter((e) => e.id !== id), guild);
  logger.info(`[emoji] ${guild.id}: deleted ${id} :${name}: by ${interaction.user.id}`);
  return safeReply(interaction, {
    embeds: [successEmbed('Emoji Deleted', `\`:${name}:\` was deleted.\n\n**Slots:** ${formatSlots(after)}`)],
    allowedMentions: NO_MENTIONS,
  });
}

// ── Entry points ──────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'add') return runAdd(interaction);
  if (sub === 'list') return runList(interaction);
  if (sub === 'delete') return runDelete(interaction);
  return null;
}

export async function autocomplete(interaction) {
  let choices = [];
  try {
    const focused = interaction.options.getFocused(true);
    if (interaction.options.getSubcommand(false) === 'delete' && focused?.name === 'emoji') {
      choices = emojiChoices(interaction.guild?.emojis?.cache ?? [], focused.value);
    }
  } catch (err) {
    logger.debug(`[emoji] autocomplete failed: ${err?.message}`);
    choices = [];
  }
  return respondAutocomplete(interaction, choices);
}
