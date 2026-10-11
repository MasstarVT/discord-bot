// Ticketing (pkg-tickets.md): panels with buttons or a dropdown, private ticket channels, claim / close / reopen / delete,
// categories, per-user open-ticket limits (race-safe under an advisory lock) and HTML transcripts.
// Everything lives here: pure helpers, the DB access for TicketPanel / TicketPanelOption / Ticket, the open flow, the lifecycle
// actions, the transcript builder, the persistent component handlers and the channelDelete hook body.
// Side-effect free at import: no timers, no I/O, no queries. Module-level state is limited to Maps, keyed mutexes and throttles
// (pkg-tickets.md §3.4); all of it may vanish on restart because every customId carries the ids it needs.
import {
  ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle, ChannelType, MessageType, ModalBuilder, OverwriteType,
  StringSelectMenuBuilder, TextInputBuilder, TextInputStyle, embedLength,
} from 'discord.js';
import { Prisma } from '@prisma/client';
import prisma from '../database/client.js';
import { ensureGuildSettings, getSettingsSafe, isModuleEnabled, peekSettings } from './settingsService.js';
import { COLORS, LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { errorEmbed, guildEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { isHttpUrl, normalizeTemplate, parseTemplateModal, renderTemplate } from '../utils/customEmbed.js';
import { renderPlaceholders, renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import {
  checkChannelUsable, isInteractionGone, missingBotPermissions, permissionNames, pingableRoleIds, safeDefer, safeReply,
} from '../utils/interactionHelpers.js';
import { clampText, formatNumber } from '../utils/text.js';
import { discordTimestamp, isValidTimeZone } from '../utils/time.js';
import {
  TimeoutError, ackBudgetMs, createKeyedMutex, createThrottle, mapLimit, sleep, withTimeout,
} from '../utils/async.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

const deepFreeze = (o) => {
  for (const v of Object.values(o)) if (v && typeof v === 'object') deepFreeze(v);
  return Object.freeze(o);
};

export const PENDING_PREFIX = 'pending:';
export const PENDING_STALE_MS = 5 * 60_000;
export const MAX_ADDED_USERS = 20;
export const MAX_TRANSCRIPT_MESSAGES = 5000;
export const TRANSCRIPT_PAGE_SIZE = 100;
export const TRANSCRIPT_DEADLINE_MS = 90_000;
export const MAX_TRANSCRIPT_BYTES = 8 * 1024 * 1024; // under Discord's 10 MiB default upload limit
export const DELETE_DELAY_MS = 5_000;
export const RENAME_WINDOW_MS = 10 * 60_000;
export const RENAME_MAX = 2;
export const CATEGORY_CHILD_LIMIT = 50;
export const PANEL_MEMO_TTL_MS = 60_000;
export const PANEL_MEMO_MAX = 500;
export const MANAGER_SYNC_MAX = 50;
export const MANAGER_SYNC_DEADLINE_MS = 60_000;
export const RECONCILE_FETCH_MAX = 50;
export const DEFAULT_CATEGORY_NAMES = Object.freeze({ open: 'Tickets', claimed: 'Claimed Tickets', closed: 'Closed Tickets' });
export const DEFAULT_OPTION = Object.freeze({ label: 'Open Ticket', emoji: '✉️', buttonStyle: 1, position: 0 });
export const BUTTON_STYLE_NAMES = Object.freeze({ 1: 'Blue', 2: 'Grey', 3: 'Green', 4: 'Red' });
export const NAME_FORMAT_TOKENS = Object.freeze(['number', 'user.name', 'user.username', 'user.id', 'user.display_name', 'topic']);

// Overwrite permission sets (PermissionFlagsBits key names; discord.js accepts these strings).
export const MEMBER_ALLOW = Object.freeze(['ViewChannel', 'SendMessages', 'ReadMessageHistory', 'AttachFiles', 'EmbedLinks']);
export const STAFF_ALLOW = MEMBER_ALLOW;
export const BOT_ALLOW = Object.freeze([...MEMBER_ALLOW, 'ManageChannels']);
export const STAFF_ALLOW_OBJ = Object.freeze(Object.fromEntries(STAFF_ALLOW.map((p) => [p, true])));
// The bot must hold every permission it grants (Discord 50013 otherwise); ManageRoles for later overwrite edits.
// ManageRoles is never put in an overwrite: only administrators may grant it in a channel.
export const OPEN_REQUIRED_PERMS = Object.freeze([
  'ViewChannel', 'ManageChannels', 'ManageRoles', 'SendMessages', 'ReadMessageHistory', 'EmbedLinks', 'AttachFiles',
]);
const CATEGORY_PERMS = Object.freeze(['ViewChannel', 'ManageChannels', 'ManageRoles']);
const TRANSCRIPT_POST_PERMS = Object.freeze(['ViewChannel', 'SendMessages', 'EmbedLinks', 'AttachFiles']);
// What the bot needs in the ticket channel for each lifecycle action (pkg-tickets.md §6.3).
const ACTION_PERMS = Object.freeze({
  claim: ['ViewChannel', 'ManageChannels', 'SendMessages'],
  close: ['ViewChannel', 'ManageChannels', 'ManageRoles', 'SendMessages'],
  reopen: ['ViewChannel', 'ManageChannels', 'ManageRoles', 'SendMessages'],
  add: ['ViewChannel', 'ManageRoles', 'SendMessages'],
  remove: ['ViewChannel', 'ManageRoles', 'SendMessages'],
  rename: ['ViewChannel', 'ManageChannels'],
  delete: ['ViewChannel', 'ManageChannels'],
});

export const DEFAULT_INTRO_TEMPLATE = deepFreeze({
  content: '{user}',
  embed: {
    title: 'Ticket #{number}',
    description: 'Thanks for reaching out, {user.display_name}! A member of our team will be with you shortly.\nPlease describe your issue in as much detail as you can.',
  },
  buttons: [],
});

// ── Raw SQL (00-overview rule 22; one entry per statement, with sample parameters) ──

const numberLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`ticket-number:${guildId}`}))`;
const nextNumberSql = (guildId) => Prisma.sql`SELECT (COALESCE(MAX("number"), 0) + 1)::int AS next FROM "Ticket" WHERE "guildId" = ${guildId}`;
const panelLockSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`ticket-panel:${guildId}`}))`;
const optionLockSql = (panelId) => Prisma.sql`SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext(${`ticket-option:${panelId}`}))`;
const panelRowLockSql = (panelId, guildId) => Prisma.sql`SELECT 1 AS ok FROM "TicketPanel" WHERE "id" = ${panelId} AND "guildId" = ${guildId} FOR UPDATE`;

export const RAW_SQL = Object.freeze([
  { name: 'tickets.numberLock', build: () => numberLockSql('0') },
  { name: 'tickets.nextNumber', build: () => nextNumberSql('0') },
  { name: 'tickets.panelLock', build: () => panelLockSql('0') },
  { name: 'tickets.optionLock', build: () => optionLockSql('0') },
  { name: 'tickets.panelRowLock', build: () => panelRowLockSql('0', '0') },
]);

// ── In-process state (pkg-tickets.md §3.4) ────────────────────────────────────

const panelMemo = new Map(); // panelId -> { guildId, at, panel }
let memoEpoch = 0; // bumped by every invalidation so a read that raced a write is not memoised
const ticketMutex = createKeyedMutex();
const panelMutex = createKeyedMutex();
const deleting = new Set();
const openThrottle = createThrottle(5_000);
const transcriptThrottle = createThrottle(60_000);
const renameLog = new Map(); // channelId -> number[] (timestamps)
const RENAME_LOG_MAX = 1000;

// ── Who may do what (pure; pkg-tickets.md §6.2) ───────────────────────────────

/**
 * @param {import('discord.js').GuildMember|null} member
 * @param {{ managerRoleIds?: string[] }|null|undefined} panel null after the panel was deleted: moderators and up only
 * @param {object|null} settings
 * @returns {boolean}
 */
export function isTicketStaff(member, panel, settings) {
  if (!member) return false;
  if (resolvePermissionLevel(member, settings) >= PERMISSION_LEVELS.MODERATOR) return true;
  const roles = panel?.managerRoleIds ?? [];
  return roles.some((id) => member.roles?.cache?.has(id));
}

/**
 * Role-based checks only; the ticket state is checked by transitionFor.
 * @param {'claim'|'close'|'reopen'|'delete'|'add'|'remove'|'transcript'|'rename'} action
 * @param {{ isOwner: boolean, isStaff: boolean }} who
 * @returns {{ ok: true } | { ok: false, code: string }}
 */
export function canPerform(action, { isOwner, isStaff }) {
  if (action === 'claim') {
    if (!isStaff) return { ok: false, code: 'NOT_STAFF' };
    if (isOwner) return { ok: false, code: 'OWN_TICKET' };
    return { ok: true };
  }
  if (action === 'close') return isOwner || isStaff ? { ok: true } : { ok: false, code: 'NOT_OWNER_OR_STAFF' };
  return isStaff ? { ok: true } : { ok: false, code: 'NOT_STAFF' };
}

/**
 * Pure state machine. `DELETED` is checked by the caller first.
 * @param {string} action
 * @param {{ status: string, claimedById?: string|null }} ticket
 * @param {{ actorId?: string, isAdmin?: boolean, source?: 'button'|'command' }} [opts]
 * @returns {{ ok: true, to: string|null, takeover?: boolean } | { ok: false, code: string, data?: object }}
 */
export function transitionFor(action, ticket, { actorId, isAdmin = false, source = 'command' } = {}) {
  const status = ticket?.status;
  switch (action) {
    case 'claim':
      if (status === 'CLOSED') return { ok: false, code: 'IS_CLOSED' };
      if (status === 'CLAIMED' && ticket.claimedById) {
        if (ticket.claimedById === actorId) return { ok: false, code: 'ALREADY_CLAIMED_SELF' };
        if (isAdmin) return { ok: true, to: 'CLAIMED', takeover: true };
        return { ok: false, code: 'ALREADY_CLAIMED', data: { claimedById: ticket.claimedById } };
      }
      return { ok: true, to: 'CLAIMED' };
    case 'close':
      return status === 'CLOSED' ? { ok: false, code: 'ALREADY_CLOSED' } : { ok: true, to: 'CLOSED' };
    case 'reopen':
      if (status !== 'CLOSED') return { ok: false, code: 'NOT_CLOSED' };
      return { ok: true, to: ticket.claimedById ? 'CLAIMED' : 'OPEN' };
    case 'delete':
      if (source === 'button' && status !== 'CLOSED') return { ok: false, code: 'CLOSE_FIRST' };
      return { ok: true, to: 'DELETED' };
    case 'add':
    case 'remove':
    case 'rename':
    case 'transcript':
      return { ok: true, to: null };
    default:
      return { ok: false, code: 'UNKNOWN' };
  }
}

// ── Numbers and channel names (pure; §6.5) ────────────────────────────────────

/** @param {number} n @returns {string} '0001'; numbers of 10000 and up print unpadded */
export const formatTicketNumber = (n) => String(n).padStart(4, '0');

/**
 * Discord-style channel name: lower case, letters (any script), digits, - and _. Keeps CJK and Cyrillic,
 * which text.js's sanitizeName would strip.
 * @param {unknown} str
 * @returns {string} possibly empty
 */
export function sanitizeChannelName(str) {
  let s = String(str ?? '').normalize('NFKC').toLowerCase();
  s = s.replace(/\s+/gu, '-').replace(/[^\p{L}\p{N}_-]/gu, '').replace(/-{2,}/g, '-').replace(/^[-_]+|[-_]+$/g, '');
  return clampText(s, 100, '');
}

/** @returns {{ ok: true, format: string } | { ok: false, error: string }} */
export function validateNameFormat(fmt) {
  const f = String(fmt ?? '').trim();
  if (!f || f.length > 90) return { ok: false, error: 'The name format must be 1–90 characters.' };
  for (const m of f.matchAll(/\{([^{}]*)\}/g)) {
    if (!NAME_FORMAT_TOKENS.includes(m[1].trim().toLowerCase())) {
      return { ok: false, error: `Unknown placeholder \`{${m[1]}}\`. Use {number}, {user.name}, {user.id}, {user.display_name} or {topic}.` };
    }
  }
  if (!sanitizeChannelName(f.replace(/\{[^{}]*\}/g, 'x'))) return { ok: false, error: 'The name format has no usable characters.' };
  return { ok: true, format: f };
}

/**
 * @param {string} format a validated name format
 * @param {{ number: number, user?: object, member?: object|null, topic?: string }} ctx
 * @returns {string} always a usable channel name
 */
export function renderChannelName(format, { number, user, member, topic } = {}) {
  const out = renderPlaceholders(format, { user, member, vars: { number: formatTicketNumber(number), topic: topic ?? '' } });
  return sanitizeChannelName(out) || `ticket-${formatTicketNumber(number)}`;
}

/** Trimmed, 1-50 characters, one line. @returns {string|null} null when invalid */
export function normalizePanelName(name) {
  const n = String(name ?? '').trim();
  return n.length >= 1 && n.length <= 50 && !/[\r\n]/.test(n) ? n : null;
}

// ── Option emoji (pure; §6.6) ─────────────────────────────────────────────────

// U+FE0F = variation selector-16, U+20E3 = combining keycap, U+200D = zero-width joiner (escapes on purpose).
const UNICODE_EMOJI_RE = /^(?:\p{Regional_Indicator}{2}|[#*0-9]️?⃣|(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:️|\p{Emoji_Modifier})?(?:[\u{E0020}-\u{E007E}]+\u{E007F})?(?:‍(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:️|\p{Emoji_Modifier})?)*)$/u;

/** @returns {boolean} true when the string is one Unicode emoji */
export const isUnicodeEmoji = (s) => typeof s === 'string' && UNICODE_EMOJI_RE.test(s);

let segmenter = null;
const getSegmenter = () => (segmenter ??= new Intl.Segmenter('en', { granularity: 'grapheme' }));
const valuesOf = (coll) => (coll && typeof coll.values === 'function' ? [...coll.values()] : []);

/**
 * @param {unknown} input
 * @param {{ client?: object, guild?: object }} [ctx]
 * @returns {{ ok: true, emoji: string|null } | { ok: false, error: string }}
 */
export function parseOptionEmoji(input, { client, guild } = {}) {
  const s = String(input ?? '').trim();
  if (s === '' || s.toLowerCase() === 'none') return { ok: true, emoji: null };
  // \w{1,32}: Discord requires 2+ characters, but the id is what the cache check below really vouches for.
  const custom = /^<(a?):(\w{1,32}):(\d{17,20})>$/.exec(s);
  if (custom) {
    if (client?.emojis?.cache?.has(custom[3])) return { ok: true, emoji: `<${custom[1]}:${custom[2]}:${custom[3]}>` };
    return { ok: false, error: "I can't use that emoji. Use a standard emoji or one from a server I'm in." };
  }
  const named = /^:(\w{2,32}):$/.exec(s);
  if (named) {
    const lower = named[1].toLowerCase();
    const found = valuesOf(guild?.emojis?.cache).find((e) => String(e.name).toLowerCase() === lower);
    return found ? { ok: true, emoji: found.toString() } : { ok: false, error: `I can't find a server emoji called :${named[1]}:.` };
  }
  const segments = [...getSegmenter().segment(s)];
  if (segments.length === 1 && isUnicodeEmoji(segments[0].segment)) return { ok: true, emoji: s };
  return { ok: false, error: `"${clampText(s, 40)}" is not a single emoji.` };
}

// ── Row classification and the rename window (pure; §6.7.4, §6.8.5) ───────────

/**
 * Sorts a member's OPEN/CLAIMED rows into live, abandoned placeholders and rows whose channel is gone.
 * `hasChannel` must mean "not confirmed gone": a cache miss alone is never a deletion (00-overview rule 20).
 * @param {Array<{ id: string, channelId: string, createdAt: Date|number }>} rows
 * @param {(channelId: string) => boolean} hasChannel
 * @param {number} now
 * @returns {{ live: object[], stalePending: string[], staleGone: string[] }} the two stale buckets hold row ids
 */
export function classifyTicketRows(rows, hasChannel, now) {
  const live = [];
  const stalePending = [];
  const staleGone = [];
  for (const row of rows ?? []) {
    if (String(row.channelId).startsWith(PENDING_PREFIX)) {
      const created = row.createdAt instanceof Date ? row.createdAt.getTime() : Number(row.createdAt);
      if (now - created < PENDING_STALE_MS) live.push(row);
      else stalePending.push(row.id);
    } else if (hasChannel(row.channelId)) live.push(row);
    else staleGone.push(row.id);
  }
  return { live, stalePending, staleGone };
}

/**
 * Our copy of Discord's 2-renames-per-10-minutes limit.
 * @param {number[]} timestamps
 * @param {number} now
 * @returns {{ allowed: boolean, retryAt: Date|null }}
 */
export function renameWindowCheck(timestamps, now) {
  const recent = (timestamps ?? []).filter((t) => now - t < RENAME_WINDOW_MS);
  if (recent.length >= RENAME_MAX) return { allowed: false, retryAt: new Date(Math.min(...recent) + RENAME_WINDOW_MS) };
  return { allowed: true, retryAt: null };
}

export function renameWindow(channelId, now = Date.now()) {
  const kept = (renameLog.get(channelId) ?? []).filter((t) => now - t < RENAME_WINDOW_MS);
  if (kept.length) renameLog.set(channelId, kept);
  else renameLog.delete(channelId);
  return renameWindowCheck(kept, now);
}

export function recordRename(channelId, now = Date.now()) {
  const kept = (renameLog.get(channelId) ?? []).filter((t) => now - t < RENAME_WINDOW_MS);
  kept.push(now);
  renameLog.delete(channelId);
  renameLog.set(channelId, kept);
  while (renameLog.size > RENAME_LOG_MAX) renameLog.delete(renameLog.keys().next().value);
}

// ── Overwrites, managers and categories (pure; §6.7.3, §6.7.5) ────────────────

/**
 * Manager roles that still exist (deleted roles never reach Discord).
 * @param {{ managerRoleIds?: string[] }|null|undefined} panel
 * @param {{ id: string, roles?: { cache?: any } }} guild
 * @returns {string[]}
 */
export function liveManagerRoleIds(panel, guild) {
  return (panel?.managerRoleIds ?? []).filter((id) => id !== guild.id && guild.roles?.cache?.has(id));
}

/**
 * Overwrites for a ticket channel. Every entry carries an explicit type (raw ids are not resolvable without it).
 * @returns {Array<{ id: string, type: number, allow?: string[], deny?: string[] }>}
 */
export function buildTicketOverwrites({ guildId, botId, ownerId, managerRoleIds = [], addedUserIds = [], closed = false }) {
  const member = () => ({
    allow: closed ? ['ViewChannel', 'ReadMessageHistory'] : [...MEMBER_ALLOW],
    deny: closed ? ['SendMessages'] : [],
  });
  const all = [
    { id: guildId, type: OverwriteType.Role, deny: ['ViewChannel'] },
    { id: botId, type: OverwriteType.Member, allow: [...BOT_ALLOW] },
    { id: ownerId, type: OverwriteType.Member, ...member() },
    ...managerRoleIds.map((id) => ({ id, type: OverwriteType.Role, allow: [...STAFF_ALLOW] })),
    ...addedUserIds.map((id) => ({ id, type: OverwriteType.Member, ...member() })),
  ];
  const seen = new Set();
  return all.filter((o) => (seen.has(o.id) ? false : (seen.add(o.id), true)));
}

/** @returns {string|null} the configured category id for a kind (null = not set) */
export function resolveCategoryId(kind, { panel, option } = {}) {
  if (kind === 'open') return option?.categoryId ?? panel?.openCategoryId ?? null;
  if (kind === 'claimed') return panel?.claimedCategoryId ?? null;
  if (kind === 'closed') return panel?.closedCategoryId ?? null;
  return null;
}

/** @returns {number} channels in a category (from the cache) */
export function childCount(category) {
  const channels = category?.guild?.channels?.cache;
  if (!channels) return 0;
  let n = 0;
  for (const c of channels.values()) if (c.parentId === category.id) n++;
  return n;
}

// ── Templates (pure; §6.4.1) ──────────────────────────────────────────────────

export function defaultPanelTemplate(style) {
  return {
    content: null,
    embed: {
      title: 'How can we help?',
      description: style === 'SELECT'
        ? 'Choose a topic below to open a private ticket with our team.'
        : 'Press a button below to open a private ticket with our team.',
    },
    buttons: [],
  };
}

const escapeRegex = (s) => String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/**
 * True when the template mentions `{token}` in its content, title, description or footer (case-insensitive).
 * @param {unknown} tpl
 * @param {string} token without braces
 */
export function templateUsesToken(tpl, token) {
  const t = normalizeTemplate(tpl);
  const re = new RegExp(`\\{\\s*${escapeRegex(token)}\\s*\\}`, 'i');
  return [t.content, t.embed?.title, t.embed?.description, t.embed?.footer?.text].some((s) => typeof s === 'string' && re.test(s));
}

const MODAL_MAX = Object.freeze({ content: 2000, title: 256, description: 4000, footer: 2048 });

/**
 * A copy of the template cut to the form's input limits (Discord rejects a modal whose prefill is longer).
 * The colour stays an integer: buildTemplateModal formats it.
 * @param {unknown} tpl
 */
export function clampTemplateForModal(tpl) {
  const t = structuredClone(normalizeTemplate(tpl));
  if (t.content) t.content = t.content.slice(0, MODAL_MAX.content);
  if (t.embed) {
    if (t.embed.title) t.embed.title = t.embed.title.slice(0, MODAL_MAX.title);
    if (t.embed.description) t.embed.description = t.embed.description.slice(0, MODAL_MAX.description);
    if (t.embed.footer?.text) t.embed.footer.text = t.embed.footer.text.slice(0, MODAL_MAX.footer);
  }
  return t;
}

const hasVisibleEmbed = (e) => Boolean(e && (e.title || e.description || e.fields?.length || e.author?.name || e.footer?.text || e.image || e.thumbnail));

/** An empty form resets a template to its default. A colour-only embed shows nothing, so it counts as empty. */
export function isEmptyTemplate(tpl) {
  const t = normalizeTemplate(tpl);
  return !(t.content && t.content.trim()) && !hasVisibleEmbed(t.embed);
}

// JSON-safe copy for a Prisma Json column (no undefined values).
const toJson = (tpl) => JSON.parse(JSON.stringify(normalizeTemplate(tpl)));

// ── Panel components (pure; §6.4.2) ───────────────────────────────────────────

const byPosition = (a, b) => ((a.position ?? 0) - (b.position ?? 0)) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);

/**
 * The buttons or dropdown of a panel. A custom emoji the bot can no longer see is skipped, so Discord never
 * rejects the whole message (50035).
 * @param {{ id: string, style: 'BUTTONS'|'SELECT' }} panel
 * @param {Array<object>} options
 * @param {{ canUseEmoji?: (emoji: string) => boolean }} [opts]
 * @returns {ActionRowBuilder[]}
 */
export function buildPanelComponents(panel, options, { canUseEmoji = () => true } = {}) {
  const list = [...(options ?? [])].sort(byPosition).slice(0, LIMITS.TICKET_PANEL_OPTIONS);
  if (list.length === 0) return [];
  const emojiOf = (o) => (o.emoji && (!String(o.emoji).startsWith('<') || canUseEmoji(o.emoji)) ? o.emoji : null);

  if (panel.style === 'SELECT') {
    const menu = new StringSelectMenuBuilder()
      .setCustomId(buildCustomId('ticket-select', panel.id))
      .setPlaceholder('Select a topic…')
      .setMinValues(1)
      .setMaxValues(1)
      .addOptions(list.map((o) => {
        const emoji = emojiOf(o);
        return {
          label: clampText(o.label, 100),
          value: o.id,
          ...(o.description ? { description: clampText(o.description, 100) } : {}),
          ...(emoji ? { emoji } : {}),
        };
      }));
    return [new ActionRowBuilder().addComponents(menu)];
  }

  const rows = [];
  for (let i = 0; i < list.length && rows.length < 5; i += 5) {
    rows.push(new ActionRowBuilder().addComponents(list.slice(i, i + 5).map((o) => {
      const button = new ButtonBuilder()
        .setCustomId(buildCustomId('ticket', 'open', panel.id, o.id))
        .setLabel(clampText(o.label, 80))
        .setStyle([1, 2, 3, 4].includes(o.buttonStyle) ? o.buttonStyle : 1);
      const emoji = emojiOf(o);
      if (emoji) button.setEmoji(emoji);
      return button;
    })));
  }
  return rows;
}

/** Close and Claim, under the intro message. With `claimed` the Claim button is disabled and says so. */
export function buildIntroRow(ticketId, { claimed = false } = {}) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('ticket', 'close', ticketId)).setLabel('Close').setEmoji('🔒').setStyle(ButtonStyle.Danger),
    new ButtonBuilder().setCustomId(buildCustomId('ticket', 'claim', ticketId)).setLabel(claimed ? 'Claimed' : 'Claim').setEmoji('🙋')
      .setStyle(ButtonStyle.Success).setDisabled(claimed),
  );
}

/** Reopen, Transcript and Delete, under the close notice. */
export function buildClosedRow(ticketId) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('ticket', 'reopen', ticketId)).setLabel('Reopen').setEmoji('🔓').setStyle(ButtonStyle.Secondary),
    new ButtonBuilder().setCustomId(buildCustomId('ticket', 'transcript', ticketId)).setLabel('Transcript').setEmoji('📄').setStyle(ButtonStyle.Secondary),
    new ButtonBuilder().setCustomId(buildCustomId('ticket', 'delete', ticketId)).setLabel('Delete').setEmoji('🗑️').setStyle(ButtonStyle.Danger),
  );
}

/**
 * JSON copies of message rows with the buttons matching `predicate` disabled; other components are kept.
 * @param {Array<object>} messageRows `interaction.message.components`
 * @param {(component: object) => boolean} [predicate] receives the API-shaped component (`custom_id`, ...)
 */
export function disableButtons(messageRows, predicate = () => true) {
  return (messageRows ?? []).map((row) => {
    const json = typeof row?.toJSON === 'function' ? row.toJSON() : structuredClone(row);
    const components = (json.components ?? []).map((c) => (c.type === 2 && c.style !== 5 && predicate(c) ? { ...c, disabled: true } : c));
    return { ...json, components };
  });
}

// ── The reason form (§6.7.1) ──────────────────────────────────────────────────

export function buildReasonModal(panelId, optionId, label) {
  return new ModalBuilder()
    .setCustomId(buildCustomId('ticket-modal', panelId, optionId))
    .setTitle(clampText(`Open a ticket: ${label}`, 45))
    .addComponents(new ActionRowBuilder().addComponents(
      new TextInputBuilder()
        .setCustomId('ticket-reason')
        .setLabel('Why are you opening this ticket?')
        .setStyle(TextInputStyle.Paragraph)
        .setRequired(true)
        .setMinLength(1)
        .setMaxLength(1000)
        .setPlaceholder('Describe your issue…'),
    ));
}

// ── Discord errors (pure; §6.13) ──────────────────────────────────────────────

/** @returns {boolean} true when channel creation failed because the parent category holds 50 channels */
export function isCategoryFullError(err) {
  return err?.code === 50035 && JSON.stringify(err?.rawError?.errors ?? {}).includes('CHANNEL_PARENT_MAX_CHANNELS');
}

/** @returns {string} a short human reason for a Discord API error */
export function describeDiscordError(err) {
  if (err instanceof TimeoutError || err?.name === 'TimeoutError') return 'Discord took too long to answer';
  if (isCategoryFullError(err)) return 'the category is full (50 channels)';
  switch (err?.code) {
    case 50013:
    case 50001: return "I'm missing permissions there";
    case 10003: return 'the channel no longer exists';
    case 10008: return 'the message no longer exists';
    case 30013: return 'the server has reached 500 channels';
    case 50035: return 'Discord rejected the request';
    case 50007: return 'the user does not accept DMs';
    default: break;
  }
  if (err?.status === 429 || err?.name === 'RateLimitError') return 'Discord is rate limiting me';
  return `Discord error ${err?.code ?? err?.status ?? 'unknown'}`;
}

// ── Messages (§4.3) ───────────────────────────────────────────────────────────

const E = (embed) => ({ embeds: [embed], allowedMentions: { parse: [] } });
const channelRef = (id) => (String(id).startsWith(PENDING_PREFIX) ? '(being created)' : `<#${id}>`);

/**
 * Maps a service result (`{ ok: false, code, data }`) to a reply payload (no `ephemeral`: the caller sets it).
 * `BOT_PERMS` is the open wording (O4), or the lifecycle wording (L13) when `data.channelId` is set.
 * @param {{ code?: string, data?: object }} result
 * @returns {{ embeds: import('discord.js').EmbedBuilder[], allowedMentions: object }}
 */
export function errorPayload(result) {
  const code = result?.code ?? 'UNKNOWN';
  const d = result?.data ?? {};
  switch (code) {
    case 'PANEL_GONE': return E(errorEmbed('Panel Not Found', 'This ticket panel no longer exists. Ask a server admin to set it up again.'));
    case 'OPTION_GONE': return E(errorEmbed('Option Not Found', 'This option was removed. Ask a server admin to republish the panel.'));
    case 'OPEN_LIMIT': {
      const ids = d.ids ?? [];
      return E(warnEmbed('Ticket Already Open', d.max === 1
        ? `You already have an open ticket: ${channelRef(ids[0])}`
        : `You already have ${ids.length} open tickets here (max ${d.max}): ${ids.map(channelRef).join(', ')}`));
    }
    case 'BOT_PERMS':
      if (d.channelId) return E(errorEmbed('Missing Permissions', `I need **${d.names}** in <#${d.channelId}> to do that.`));
      return E(errorEmbed('Missing Permissions', `I need **${d.names}**${d.where ? ` in **${d.where}**` : ''} to open tickets. Ask a server admin to fix my permissions.`));
    case 'GUILD_FULL': return E(errorEmbed('Server Is Full', "This server has reached Discord's limit of 500 channels, so I can't open a ticket. Ask an admin to delete old channels."));
    case 'CREATE_FAILED': return E(errorEmbed('Could Not Open Ticket', `Discord refused to create the channel: ${d.reason}. Please try again later.`));
    case 'SLOW': return E(errorEmbed('Try Again', 'Ticketing is a bit slow right now. Please try again in a moment.'));
    case 'THROTTLED': return E(warnEmbed('Slow Down', 'You just opened a ticket. Please wait a few seconds.'));
    case 'MODULE_OFF': return E(warnEmbed('Module Disabled', 'The **Ticketing** module is turned off on this server.'));
    case 'DB_ERROR': return E(errorEmbed('Could Not Open Ticket', 'Something went wrong while saving your ticket. Please try again.'));
    case 'NOT_TICKET': return E(errorEmbed('Not a Ticket', 'Run this command inside a ticket channel.'));
    case 'TICKET_NOT_FOUND': return E(errorEmbed('Ticket Not Found', 'This ticket no longer exists.'));
    case 'TICKET_DELETED': return E(errorEmbed('Ticket Deleted', 'This ticket was already deleted. You can delete this channel manually.'));
    case 'NOT_STAFF': return E(errorEmbed('Staff Only', 'Only ticket managers and moderators can do that.'));
    case 'NOT_OWNER_OR_STAFF': return E(errorEmbed('Not Allowed', 'Only the person who opened this ticket or the ticket staff can close it.'));
    case 'OWN_TICKET': return E(warnEmbed('Not Allowed', "You can't claim your own ticket."));
    case 'ALREADY_CLAIMED_SELF': return E(warnEmbed('Already Claimed', 'You already claimed this ticket.'));
    case 'ALREADY_CLAIMED': return E(warnEmbed('Already Claimed', `This ticket is already claimed by <@${d.claimedById}>.`));
    case 'IS_CLOSED': return E(warnEmbed('Ticket Closed', 'This ticket is closed. Reopen it first.'));
    case 'ALREADY_CLOSED': return E(warnEmbed('Already Closed', 'This ticket is already closed.'));
    case 'NOT_CLOSED': return E(warnEmbed('Not Closed', 'This ticket is not closed.'));
    case 'CLOSE_FIRST': return E(warnEmbed('Close It First', 'Close the ticket before deleting it with this button, or use `/ticket delete`.'));
    case 'CONFLICT': return E(warnEmbed('Try Again', 'This ticket was changed by someone else just now. Please try again.'));
    case 'DELETING': return E(warnEmbed('Deleting', 'This ticket is already being deleted.'));
    case 'RENAME_LIMIT': return E(warnEmbed('Too Many Renames', `Discord allows only 2 renames per channel every 10 minutes. Try again ${discordTimestamp(d.retryAt, 'R')}.`));
    case 'BAD_NAME': return E(errorEmbed('Invalid Name', 'That name has no usable characters. Use letters, numbers, - or _.'));
    case 'ADD_BOT': return E(errorEmbed('Not Allowed', "Bots can't be added to tickets."));
    case 'NOT_MEMBER': return E(errorEmbed('Member Not Found', "That user isn't in this server."));
    case 'IS_OWNER':
      return d.op === 'remove'
        ? E(errorEmbed('Not Allowed', "You can't remove the person who opened the ticket. Close the ticket instead."))
        : E(warnEmbed('Already In Ticket', 'The ticket opener already has access.'));
    case 'ALREADY_ADDED': return E(warnEmbed('Already Added', `<@${d.userId}> already has access to this ticket.`));
    case 'ADDED_LIMIT': return E(errorEmbed('Too Many Members', `A ticket can have at most ${MAX_ADDED_USERS} added members.`));
    case 'NOT_ADDED': return E(warnEmbed('Not Added', `<@${d.userId}> was not added to this ticket with \`/ticket add\`.`));
    case 'TRANSCRIPT_THROTTLED': return E(warnEmbed('Please Wait', 'A transcript of this ticket was made less than a minute ago. Try again shortly.'));
    case 'TRANSCRIPT_FAILED': return E(errorEmbed('Transcript Failed', `I couldn't read this channel's history: ${d.reason}.`));
    case 'TRANSCRIPT_CHANNEL':
      return E(errorEmbed('Transcript Channel Unusable', `I can't post transcripts in <#${d.channelId}> (${d.reason}). Fix my permissions there or turn channel transcripts off with \`/ticket-panel transcripts\`. The ticket was not deleted.`));
    case 'TRANSCRIPT_ABORT': return E(errorEmbed('Transcript Failed', `I couldn't save the transcript (${d.reason}), so the ticket was not deleted.`));
    case 'DELETE_FAILED':
      return E(errorEmbed('Delete Failed', `I couldn't delete this channel: ${d.reason}.${d.transcriptSaved ? ' The transcript was already saved.' : ''}`));
    default: return E(errorEmbed('Something Went Wrong', 'That did not work. Please try again.'));
  }
}

const permLine = (u, id) => (u.missing?.length ? `I need **${permissionNames(u.missing).join(', ')}** in <#${id}>.` : `${u.reason}.`);

/**
 * Panel-admin messages (P1-P27). `data` fills the placeholders.
 * @param {string} code
 * @param {object} [d]
 */
export function panelErrorPayload(code, d = {}) {
  switch (code) {
    case 'NAME_TAKEN': return E(errorEmbed('Name Taken', `A panel called **${d.name}** already exists.`));
    case 'PANEL_LIMIT': return E(errorEmbed('Too Many Panels', `This server already has ${LIMITS.TICKET_PANELS} ticket panels. Delete one first.`));
    case 'PANEL_NOT_FOUND': return E(errorEmbed('Panel Not Found', 'Pick a panel from the list.'));
    case 'BAD_FORMAT': return E(errorEmbed('Invalid Name Format', d.error));
    case 'BAD_PANEL_NAME': return E(errorEmbed('Invalid Name', 'Panel names must be 1–50 characters on one line.'));
    case 'CHANNEL_UNUSABLE': return E(errorEmbed('Cannot Post There', permLine(d.u, d.id)));
    case 'TRANSCRIPT_CHANNEL_UNUSABLE': return E(errorEmbed('Cannot Post Transcripts There', permLine(d.u, d.id)));
    case 'PERMS_SERVER':
      return E(errorEmbed('Missing Permissions', `To open tickets I need **${permissionNames(d.missing).join(', ')}** on this server. Fix my role, then publish again.`));
    case 'PUBLISH_FAILED': return E(errorEmbed('Could Not Publish', `I couldn't post the panel: ${d.reason}.`));
    case 'BAD_LABEL': return E(errorEmbed('Invalid Label', 'Option labels must be 1–80 characters.'));
    case 'OPTION_LIMIT': return E(errorEmbed('Too Many Options', `A panel can have at most ${LIMITS.TICKET_PANEL_OPTIONS} options.`));
    case 'OPTION_DUP': return E(errorEmbed('Duplicate Option', `This panel already has an option called **${d.label}**.`));
    case 'BAD_EMOJI': return E(errorEmbed('Invalid Emoji', d.error));
    case 'OPTION_NOT_FOUND': return E(errorEmbed('Option Not Found', 'Pick an option from the list.'));
    case 'LAST_OPTION': return E(errorEmbed('Last Option', 'A panel needs at least one option. Add another option before removing this one.'));
    case 'EVERYONE_ROLE': return E(errorEmbed('Invalid Role', '@everyone cannot be a Ticket Manager role.'));
    case 'ALREADY_MANAGER': return E(warnEmbed('Already a Manager', `<@&${d.roleId}> is already a Ticket Manager role of **${d.panel}**.`));
    case 'NOT_MANAGER': return E(warnEmbed('Not a Manager', `<@&${d.roleId}> is not a Ticket Manager role of **${d.panel}**.`));
    case 'MANAGER_LIMIT': return E(errorEmbed('Too Many Manager Roles', `A panel can have at most ${LIMITS.TICKET_MANAGER_ROLES} Ticket Manager roles.`));
    case 'CATEGORY_PERMS': return E(errorEmbed('Missing Permissions', `I need **${d.names}** in the **${d.category}** category.`));
    case 'CONFLICT_CHANNEL': return E(errorEmbed('Conflicting Options', 'Use either `channel` or `disable:true`, not both.'));
    case 'CONFLICT_CATEGORY': return E(errorEmbed('Conflicting Options', 'Use either `category` or `clear_category:true`, not both.'));
    case 'INVALID_MESSAGE': return E(errorEmbed('Invalid Message', (d.errors ?? []).map((e) => `• ${e}`).join('\n')));
    case 'BAD_COMBO': return E(errorEmbed('Invalid Options', 'The `option` setting only works with `type: Ticket intro`.'));
    case 'NOT_ADMIN': return E(errorEmbed('Insufficient Permissions', 'You need the Admin level to edit ticket panels.'));
    case 'FORM_EXPIRED': return E(errorEmbed('Form Expired', 'This form is no longer valid. Run `/ticket-panel message` again.'));
    case 'PANEL_DELETED': return E(errorEmbed('Panel Not Found', 'This panel was deleted while the form was open.'));
    case 'SLOW': return E(errorEmbed('Try Again', 'Ticketing is a bit slow right now. Please try again in a moment.'));
    default: return E(errorEmbed('Something Went Wrong', 'That did not work. Please try again.'));
  }
}

/** The P21 line for a failed panel refresh, or '' when there is nothing to say. */
export function refreshNote(r) {
  if (!r || r.ok) return '';
  if (r.code === 'GONE') return '\n⚠️ The published panel message was deleted. Run `/ticket-panel publish` to post it again.';
  if (r.code === 'NO_ACCESS') return `\n⚠️ I couldn't update the published panel in <#${r.channelId}> (missing access).`;
  return `\n⚠️ I couldn't update the published panel (${r.reason ?? 'unknown error'}).`;
}

export const MANAGERS_WARNING = '⚠️ This panel has no Ticket Manager roles, so only moderators and admins can see its tickets. Add one with `/ticket-panel managers add`.';

// ── Panel memo and DB reads (§3.2, §3.4) ──────────────────────────────────────

const PANEL_INCLUDE = Object.freeze({ options: { orderBy: [{ position: 'asc' }, { id: 'asc' }] } });

export function invalidatePanel(panelId) {
  memoEpoch++;
  panelMemo.delete(panelId);
}

export function invalidateGuildPanels(guildId) {
  memoEpoch++;
  for (const [id, v] of panelMemo) if (v.guildId === guildId) panelMemo.delete(id);
}

/** Memo-only, synchronous: the panel (with ordered options) when it was read in the last 60 s, else null. */
export function peekPanel(guildId, panelId, now = Date.now()) {
  const hit = panelMemo.get(panelId);
  if (!hit || hit.guildId !== guildId || now - hit.at >= PANEL_MEMO_TTL_MS) return null;
  return hit.panel;
}

/** The panel for the open path: memo, then the database. A missing panel is not memoised. */
export async function getPanelForOpen(guildId, panelId) {
  const hit = peekPanel(guildId, panelId);
  if (hit) return hit;
  const epoch = memoEpoch;
  const panel = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId }, include: PANEL_INCLUDE });
  if (panel && epoch === memoEpoch) {
    panelMemo.delete(panelId);
    panelMemo.set(panelId, { guildId, at: Date.now(), panel });
    while (panelMemo.size > PANEL_MEMO_MAX) panelMemo.delete(panelMemo.keys().next().value);
  }
  return panel;
}

/** A panel by id or (case-insensitive) name, with its ordered options; null when none matches. */
export function findPanel(guildId, ref) {
  const r = String(ref ?? '');
  return prisma.ticketPanel.findFirst({
    where: { guildId, OR: [{ id: r }, { name: { equals: r, mode: 'insensitive' } }] },
    include: PANEL_INCLUDE,
  });
}

export function findPanelsForAutocomplete(guildId, q) {
  return prisma.ticketPanel.findMany({
    where: { guildId, ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}) },
    orderBy: { name: 'asc' },
    take: 25,
    select: { id: true, name: true, style: true },
  });
}

export function findOptionsForAutocomplete(guildId, panelRef, q) {
  const r = String(panelRef ?? '');
  return prisma.ticketPanelOption.findMany({
    where: {
      panel: { guildId, OR: [{ id: r }, { name: { equals: r, mode: 'insensitive' } }] },
      ...(q ? { label: { contains: q, mode: 'insensitive' } } : {}),
    },
    orderBy: [{ position: 'asc' }, { id: 'asc' }],
    take: 25,
    select: { id: true, label: true, emoji: true },
  });
}

export function getTicketByChannel(guildId, channelId) {
  return prisma.ticket.findFirst({ where: { guildId, channelId }, include: { panel: true, option: true } });
}

export function getTicketById(guildId, ticketId) {
  return prisma.ticket.findFirst({ where: { id: ticketId, guildId }, include: { panel: true, option: true } });
}

// ── Channel lookups: a cache miss is not a deletion (00-overview rule 20) ─────

const loadSettings = async (guildId) => peekSettings(guildId) ?? getSettingsSafe(guildId);

/**
 * Finds a channel: the cache first, then a REST fetch. `gone` only when Discord answered 10003 while the guild is
 * available; a timeout or any other error is `unknown` and callers treat it as present.
 * @param {import('discord.js').Guild} guild
 * @param {string} id
 * @returns {Promise<{ channel: import('discord.js').GuildBasedChannel|null, state: 'present'|'gone'|'unknown' }>}
 */
export async function resolveChannel(guild, id, { timeoutMs = 4000 } = {}) {
  const cached = guild.channels.cache.get(id);
  if (cached) return { channel: cached, state: 'present' };
  if (guild.available === false) return { channel: null, state: 'unknown' };
  try {
    const fetched = await withTimeout(guild.channels.fetch(id), timeoutMs, 'ticket channel fetch');
    return fetched ? { channel: fetched, state: 'present' } : { channel: null, state: 'unknown' };
  } catch (err) {
    if (err?.code === 10003 && guild.available !== false) return { channel: null, state: 'gone' };
    return { channel: null, state: 'unknown' };
  }
}

/**
 * The ids (of those not in the cache) that Discord confirmed are gone.
 * @param {import('discord.js').Guild} guild
 * @param {string[]} ids
 * @returns {Promise<Set<string>>}
 */
export async function confirmGoneChannels(guild, ids) {
  const gone = new Set();
  const missing = [...new Set(ids)].filter((id) => !guild.channels.cache.has(id));
  const results = await mapLimit(missing, 5, (id) => resolveChannel(guild, id));
  results.forEach((r, i) => { if (r.status === 'fulfilled' && r.value.state === 'gone') gone.add(missing[i]); });
  return gone;
}

// ── Categories (§6.7.3) ───────────────────────────────────────────────────────

const CATEGORY_FIELDS = Object.freeze({ open: 'openCategoryId', claimed: 'claimedCategoryId', closed: 'closedCategoryId' });
const isCategory = (c) => c?.type === ChannelType.GuildCategory;
const FULL_NOTE = 'The ticket category is full (50 channels), so this ticket was created outside it.';

/**
 * Finds, reuses or creates the category for a kind and stores it on the panel. Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {string} panelId
 * @param {'open'|'claimed'|'closed'} kind
 * @returns {Promise<{ category: import('discord.js').CategoryChannel|null, created: boolean, reused: boolean, note: string|null }>}
 */
export function ensureCategory(guild, panelId, kind) {
  return panelMutex.run(`${panelId}:cat`, async () => {
    const none = (note = null) => ({ category: null, created: false, reused: false, note });
    try {
      const field = CATEGORY_FIELDS[kind];
      const panel = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId: guild.id } });
      if (!panel) return none();

      const current = panel[field];
      if (current) {
        const r = await resolveChannel(guild, current);
        if (isCategory(r.channel)) return { category: r.channel, created: false, reused: false, note: null };
        if (r.state === 'gone') {
          await prisma.ticketPanel.updateMany({ where: { id: panelId, [field]: current }, data: { [field]: null } });
          invalidatePanel(panelId);
        } else if (r.state === 'unknown') {
          return none("I couldn't check the ticket category right now.");
        }
      }

      const wanted = DEFAULT_CATEGORY_NAMES[kind];
      const existing = valuesOf(guild.channels.cache).find((c) => isCategory(c) && c.name.toLowerCase() === wanted.toLowerCase());
      let category = existing ?? null;
      let created = false;
      if (!category) {
        if (missingBotPermissions(guild, ['ManageChannels', 'ManageRoles', 'ViewChannel']).length) {
          return none(`I couldn't create the "${wanted}" category (I'm missing permissions).`);
        }
        try {
          category = await guild.channels.create({
            name: wanted,
            type: ChannelType.GuildCategory,
            permissionOverwrites: [
              { id: guild.id, type: OverwriteType.Role, deny: ['ViewChannel'] },
              { id: guild.client.user.id, type: OverwriteType.Member, allow: ['ViewChannel', 'ManageChannels'] },
              ...liveManagerRoleIds(panel, guild).map((id) => ({ id, type: OverwriteType.Role, allow: ['ViewChannel'] })),
            ],
            reason: 'Ticket category created on demand',
          });
          created = true;
        } catch (err) {
          return none(err?.code === 30013
            ? `This server has 500 channels, so I couldn't create the "${wanted}" category.`
            : `I couldn't create the "${wanted}" category (${describeDiscordError(err)}).`);
        }
      }

      const stored = await prisma.ticketPanel.updateMany({ where: { id: panelId, [field]: null }, data: { [field]: category.id } });
      if (stored.count === 0) {
        const fresh = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId: guild.id }, select: { [field]: true } });
        const winner = fresh?.[field] ? await resolveChannel(guild, fresh[field]) : null;
        if (winner && isCategory(winner.channel)) {
          logger.debug(`tickets: another writer stored the ${kind} category of panel ${panelId}`);
          category = winner.channel;
          created = false;
        }
      }
      invalidatePanel(panelId);
      return { category, created, reused: !created, note: null };
    } catch (err) {
      logger.warn('tickets: ensureCategory failed', err);
      return none("I couldn't set up the ticket category.");
    }
  });
}

// Checks a resolved category: permissions and room. `missing` non-empty means the bot cannot use it.
function finishParent(category, note, fullNote) {
  if (!category) return { category: null, note, missing: null };
  const missing = missingBotPermissions(category, CATEGORY_PERMS);
  if (missing.length) return { category, note, missing };
  if (childCount(category) >= CATEGORY_CHILD_LIMIT) {
    return { category: null, note: typeof fullNote === 'function' ? fullNote(category) : fullNote, missing: null };
  }
  return { category, note, missing: null };
}

const fullCategoryNote = (c) => `**${c.name}** is full (50 channels), so I left the ticket where it is.`;

/**
 * The parent for a new (or reopened) ticket: the option's category, else the panel's open category (created on demand).
 * @param {{ fullNote?: string|((category: object) => string) }} [opts] the note when the category already holds 50 channels
 * @returns {Promise<{ category: object|null, note: string|null, missing: string[]|null }>}
 */
export async function resolveOpenParent(guild, panel, option, { fullNote = FULL_NOTE } = {}) {
  let category = null;
  let note = null;
  if (option?.categoryId) {
    const r = await resolveChannel(guild, option.categoryId);
    if (isCategory(r.channel)) category = r.channel;
    else if (r.state === 'gone') {
      await prisma.ticketPanelOption.updateMany({ where: { id: option.id }, data: { categoryId: null } }).catch((err) => logger.warn('tickets: clearing a deleted option category failed', err));
      if (panel) invalidatePanel(panel.id);
    }
  }
  if (!category && panel) {
    const e = await ensureCategory(guild, panel.id, 'open');
    category = e.category;
    note = e.note;
  }
  return finishParent(category, note, fullNote);
}

async function resolveKindParent(guild, panelId, kind) {
  if (!panelId) return { category: null, note: null, missing: null };
  const e = await ensureCategory(guild, panelId, kind);
  return finishParent(e.category, e.note, fullCategoryNote);
}

// Moves a ticket channel into the resolved parent. lockPermissions: false is mandatory: discord.js defaults to true, which would
// replace the ticket's private overwrites with the category's.
async function moveChannel(channel, parent, reason) {
  if (!parent.category) return { moved: false, note: parent.note };
  const name = parent.category.name;
  if (parent.missing) return { moved: false, note: `I can't manage channels in the **${name}** category, so I didn't move the ticket.` };
  if (channel.parentId === parent.category.id) return { moved: false, note: null };
  try {
    await channel.setParent(parent.category.id, { lockPermissions: false, reason: clampText(reason, 512) });
    return { moved: true, note: null };
  } catch (err) {
    if (isCategoryFullError(err)) return { moved: false, note: `**${name}** is full (50 channels), so I didn't move the ticket.` };
    return { moved: false, note: `I couldn't move the ticket to **${name}** (${describeDiscordError(err)}).` };
  }
}

// ── Panel rendering (§6.4) ────────────────────────────────────────────────────

/**
 * @param {object} panel a panel with its options
 * @param {{ guild: object, channel?: object, settings?: object|null, client?: object }} ctx
 */
export function buildPanelPayload(panel, { guild, channel, settings, client }) {
  const emojis = (client ?? guild?.client)?.emojis?.cache;
  const canUseEmoji = (str) => {
    const m = /:(\d{17,20})>$/.exec(str);
    return m ? Boolean(emojis?.has(m[1])) : true;
  };
  const opts = {
    defaultColor: settings?.embedColor ?? COLORS.INFO,
    allowedMentions: { parse: [] },
    extraComponents: buildPanelComponents(panel, panel.options, { canUseEmoji }),
  };
  const ctx = { guild, channel };
  const base = panel.template ? normalizeTemplate(panel.template) : defaultPanelTemplate(panel.style);
  return renderTemplate({ ...base, buttons: [] }, ctx, opts) ?? renderTemplate(defaultPanelTemplate(panel.style), ctx, opts);
}

const isMissing = (err) => err?.code === 10008 || err?.code === 10003;

async function clearPanelMessage(panelId, guildId) {
  await prisma.ticketPanel.updateMany({ where: { id: panelId, guildId }, data: { channelId: null, messageId: null } });
  invalidatePanel(panelId);
}

/**
 * Posts the panel message in `channel`, or edits the published one when it lives there. Never throws.
 * @returns {Promise<{ ok: true, mode: 'posted'|'edited', message: object } | { ok: false, code?: string, error?: any }>}
 */
export function publishPanel({ guild, panelId, channel, settings }) {
  return panelMutex.run(panelId, async () => {
    try {
      const panel = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId: guild.id }, include: PANEL_INCLUDE });
      if (!panel) return { ok: false, code: 'PANEL_GONE' };
      const payload = buildPanelPayload(panel, { guild, channel, settings, client: guild.client });

      if (panel.messageId && panel.channelId === channel.id) {
        try {
          const msg = await channel.messages.fetch(panel.messageId);
          await msg.edit({ content: '', ...payload });
          return { ok: true, mode: 'edited', message: msg };
        } catch (err) {
          if (err?.code !== 10008) return { ok: false, error: err };
        }
      } else if (panel.messageId && panel.channelId) {
        await guild.channels.cache.get(panel.channelId)?.messages?.delete(panel.messageId).catch(() => {});
      }

      const msg = await channel.send(payload);
      try {
        await prisma.ticketPanel.updateMany({ where: { id: panelId, guildId: guild.id }, data: { channelId: channel.id, messageId: msg.id } });
      } catch (err) {
        await msg.delete().catch(() => {});
        return { ok: false, error: err };
      }
      invalidatePanel(panelId);
      return { ok: true, mode: 'posted', message: msg };
    } catch (err) {
      return { ok: false, error: err };
    }
  });
}

/**
 * Re-renders the published panel message after a change. Never throws; the caller turns a failure into a note (refreshNote).
 * @returns {Promise<{ ok: true, skipped?: boolean } | { ok: false, code: 'GONE'|'NO_ACCESS'|'ERROR', channelId?: string, reason?: string }>}
 */
export function refreshPanelMessage(guild, panelId) {
  return panelMutex.run(panelId, async () => {
    try {
      const panel = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId: guild.id }, include: PANEL_INCLUDE });
      if (!panel?.channelId || !panel.messageId) return { ok: true, skipped: true };
      const r = await resolveChannel(guild, panel.channelId);
      if (r.state === 'gone') {
        await clearPanelMessage(panelId, guild.id);
        return { ok: false, code: 'GONE' };
      }
      if (!r.channel) return { ok: false, code: 'ERROR', reason: "I couldn't reach the channel" };
      if (checkChannelUsable(r.channel, { needs: ['ViewChannel', 'SendMessages', 'EmbedLinks'] }).ok === false) {
        return { ok: false, code: 'NO_ACCESS', channelId: panel.channelId };
      }
      try {
        const settings = await loadSettings(guild.id);
        const msg = await r.channel.messages.fetch(panel.messageId);
        await msg.edit({ content: '', ...buildPanelPayload(panel, { guild, channel: r.channel, settings, client: guild.client }) });
        return { ok: true };
      } catch (err) {
        if (isMissing(err)) {
          await clearPanelMessage(panelId, guild.id);
          return { ok: false, code: 'GONE' };
        }
        if (err?.code === 50001 || err?.code === 50013) return { ok: false, code: 'NO_ACCESS', channelId: panel.channelId };
        return { ok: false, code: 'ERROR', reason: describeDiscordError(err) };
      }
    } catch (err) {
      logger.warn('tickets: refreshPanelMessage failed', err);
      return { ok: false, code: 'ERROR', reason: describeDiscordError(err) };
    }
  });
}

// ── Views (§6.4.5) ────────────────────────────────────────────────────────────

const styleName = (panel) => (panel.style === 'SELECT' ? 'Dropdown' : 'Buttons');
const mentionRoles = (ids) => (ids.length ? ids.map((id) => `<@&${id}>`).join(' ') : 'none');

function categoryLine(label, id, kind, guild) {
  if (!id) return `**${label}:** not set — I create "${DEFAULT_CATEGORY_NAMES[kind]}" when it is first needed`;
  const cat = guild.channels.cache.get(id);
  if (cat) return `**${label}:** <#${id}> (${childCount(cat)}/${CATEGORY_CHILD_LIMIT} channels)`;
  if (guild.available === false) return `**${label}:** <#${id}>`;
  return `**${label}:** deleted — I will create a new one when needed`;
}

/** The three category lines of a panel. */
export function categoriesText(panel, guild) {
  return [
    categoryLine('Open', panel.openCategoryId, 'open', guild),
    categoryLine('Claimed', panel.claimedCategoryId, 'claimed', guild),
    categoryLine('Closed', panel.closedCategoryId, 'closed', guild),
  ].join('\n');
}

/** The transcript settings of a panel. */
export function transcriptsText(panel) {
  return [
    `**Channel:** ${panel.transcriptChannelId ? `<#${panel.transcriptChannelId}>` : 'off'}`,
    `**DM the opener:** ${panel.dmTranscript ? 'Yes' : 'No'}`,
    'Transcripts are made when a ticket is deleted, and with `/ticket transcript`.',
  ].join('\n');
}

export function buildCategoriesEmbed(panel, guild, settings) {
  return guildEmbed(settings, `🎫 ${clampText(panel.name, 100)} — categories`, categoriesText(panel, guild));
}

export function buildTranscriptsEmbed(panel, settings) {
  return guildEmbed(settings, `🎫 ${clampText(panel.name, 100)} — transcripts`, transcriptsText(panel));
}

export function buildPanelDetailsEmbed(panel, guild, settings) {
  const published = panel.channelId && panel.messageId ? `<#${panel.channelId}>` : 'not published';
  const managers = liveManagerRoleIds(panel, guild);
  return guildEmbed(settings, `🎫 ${clampText(panel.name, 200)}`)
    .addFields(
      { name: 'Style', value: styleName(panel), inline: true },
      { name: 'Published', value: published, inline: true },
      { name: 'Ask for a reason', value: panel.askReason ? 'Yes' : 'No', inline: true },
      { name: 'Open tickets per member', value: String(panel.maxOpenPerUser), inline: true },
      { name: 'Channel name', value: clampText(`\`${panel.channelNameFormat}\``, 1024), inline: true },
      { name: 'Manager roles', value: managers.length ? clampText(mentionRoles(managers), 1024) : 'none — only moderators and admins', inline: false },
      { name: 'Categories', value: clampText(categoriesText(panel, guild), 1024), inline: false },
      { name: 'Transcripts', value: `${panel.transcriptChannelId ? `<#${panel.transcriptChannelId}>` : 'off'}, ${panel.dmTranscript ? 'DMs on' : 'DMs off'}`, inline: false },
      { name: 'Options', value: `${panel.options?.length ?? 0}/${LIMITS.TICKET_PANEL_OPTIONS}`, inline: true },
    );
}

/** The `option list` line of one option. */
export function optionLine(option, index, panel) {
  const style = panel.style === 'BUTTONS' ? ` — ${BUTTON_STYLE_NAMES[option.buttonStyle] ?? 'Blue'}` : '';
  return `**${index + 1}.** ${option.emoji ?? '▫️'} **${clampText(option.label, 80)}**${style}${option.description ? ` · ${clampText(option.description, 100)}` : ''}${option.categoryId ? ` · opens in <#${option.categoryId}>` : ''}`;
}

// ── Panel and option writes (§4.2) ────────────────────────────────────────────

const isUniqueViolation = (err) => err?.code === 'P2002';

/**
 * Creates a panel with its default "Open Ticket" option. The cap and the name check run under a per-guild advisory lock.
 * @returns {Promise<{ ok: true, panel: object } | { ok: false, code: 'PANEL_LIMIT'|'NAME_TAKEN', data?: object }>}
 * @throws on database failure
 */
export async function createPanel({ guildId, name, style = 'BUTTONS', askReason = false, maxOpenPerUser = 1, channelNameFormat = 'ticket-{number}' }) {
  await ensureGuildSettings(guildId);
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(panelLockSql(guildId));
      if (await tx.ticketPanel.count({ where: { guildId } }) >= LIMITS.TICKET_PANELS) return { ok: false, code: 'PANEL_LIMIT' };
      const clash = await tx.ticketPanel.findFirst({ where: { guildId, name: { equals: name, mode: 'insensitive' } }, select: { id: true } });
      if (clash) return { ok: false, code: 'NAME_TAKEN', data: { name } };
      const panel = await tx.ticketPanel.create({
        data: { guildId, name, style, askReason, maxOpenPerUser, channelNameFormat, options: { create: [{ ...DEFAULT_OPTION }] } },
        include: { options: true },
      });
      return { ok: true, panel };
    }, { timeout: 10_000 });
  } catch (err) {
    if (isUniqueViolation(err)) return { ok: false, code: 'NAME_TAKEN', data: { name } };
    throw err;
  }
}

/**
 * Writes panel columns (guild-scoped) and drops the memo. A rename that collides case-insensitively is refused.
 * @returns {Promise<{ ok: true } | { ok: false, code: 'NAME_TAKEN'|'PANEL_NOT_FOUND', data?: object }>}
 * @throws on database failure
 */
export async function updatePanelFields(panelId, guildId, data) {
  try {
    if (typeof data.name === 'string') {
      const clash = await prisma.ticketPanel.findFirst({
        where: { guildId, id: { not: panelId }, name: { equals: data.name, mode: 'insensitive' } }, select: { id: true },
      });
      if (clash) return { ok: false, code: 'NAME_TAKEN', data: { name: data.name } };
    }
    const r = await prisma.ticketPanel.updateMany({ where: { id: panelId, guildId }, data });
    return r.count ? { ok: true } : { ok: false, code: 'PANEL_NOT_FOUND' };
  } catch (err) {
    if (isUniqueViolation(err)) return { ok: false, code: 'NAME_TAKEN', data: { name: data.name } };
    throw err;
  } finally {
    invalidatePanel(panelId);
  }
}

/** Adds an option under a per-panel advisory lock (cap, duplicate label, next position). */
export async function addOption(panelId, guildId, data) {
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(optionLockSql(panelId));
      const panel = await tx.ticketPanel.findFirst({ where: { id: panelId, guildId }, select: { id: true } });
      if (!panel) return { ok: false, code: 'PANEL_NOT_FOUND' };
      const options = await tx.ticketPanelOption.findMany({ where: { panelId }, select: { id: true, label: true, position: true } });
      if (options.length >= LIMITS.TICKET_PANEL_OPTIONS) return { ok: false, code: 'OPTION_LIMIT' };
      const lower = data.label.toLowerCase();
      if (options.some((o) => o.label.toLowerCase() === lower)) return { ok: false, code: 'OPTION_DUP', data: { label: data.label } };
      const position = options.reduce((m, o) => Math.max(m, o.position), -1) + 1;
      const option = await tx.ticketPanelOption.create({ data: { ...data, panelId, position } });
      return { ok: true, option, count: options.length + 1 };
    }, { timeout: 10_000 });
  } finally {
    invalidatePanel(panelId);
  }
}

/** Edits an option under the same lock, so two renames cannot create a duplicate label. */
export async function updateOption(panelId, guildId, optionId, data) {
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(optionLockSql(panelId));
      const panel = await tx.ticketPanel.findFirst({ where: { id: panelId, guildId }, select: { id: true } });
      if (!panel) return { ok: false, code: 'PANEL_NOT_FOUND' };
      const options = await tx.ticketPanelOption.findMany({ where: { panelId }, select: { id: true, label: true } });
      if (!options.some((o) => o.id === optionId)) return { ok: false, code: 'OPTION_NOT_FOUND' };
      if (typeof data.label === 'string') {
        const lower = data.label.toLowerCase();
        if (options.some((o) => o.id !== optionId && o.label.toLowerCase() === lower)) return { ok: false, code: 'OPTION_DUP', data: { label: data.label } };
      }
      const option = await tx.ticketPanelOption.update({ where: { id: optionId }, data });
      return { ok: true, option };
    }, { timeout: 10_000 });
  } finally {
    invalidatePanel(panelId);
  }
}

/** Removes an option; a panel keeps at least one. Tickets of the option keep working with `optionId = null`. */
export async function removeOption(panelId, guildId, optionId) {
  try {
    return await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(optionLockSql(panelId));
      const panel = await tx.ticketPanel.findFirst({ where: { id: panelId, guildId }, select: { id: true } });
      if (!panel) return { ok: false, code: 'PANEL_NOT_FOUND' };
      const options = await tx.ticketPanelOption.findMany({ where: { panelId }, select: { id: true } });
      if (!options.some((o) => o.id === optionId)) return { ok: false, code: 'OPTION_NOT_FOUND' };
      if (options.length <= 1) return { ok: false, code: 'LAST_OPTION' };
      await tx.ticketPanelOption.deleteMany({ where: { id: optionId, panelId } });
      return { ok: true };
    }, { timeout: 10_000 });
  } finally {
    invalidatePanel(panelId);
  }
}

/**
 * Deletes a panel and its published message (best effort). The options cascade; tickets keep their channels with `panelId = null`.
 * @returns {Promise<{ open: number }>} how many tickets were still not deleted
 */
export async function deletePanel(guild, panel) {
  if (panel.channelId && panel.messageId) {
    const channel = guild.channels.cache.get(panel.channelId);
    await channel?.messages?.delete(panel.messageId).catch((err) => {
      if (![10003, 10008, 50001, 50013].includes(err?.code)) logger.debug(`tickets: could not delete the panel message: ${err?.message}`);
    });
  }
  const open = await prisma.ticket.count({ where: { guildId: guild.id, panelId: panel.id, status: { not: 'DELETED' } } });
  await prisma.ticketPanel.deleteMany({ where: { id: panel.id, guildId: guild.id } });
  invalidatePanel(panel.id);
  return { open };
}

/**
 * Read-modify-write of `managerRoleIds` under a row lock. Roles that no longer exist are pruned first (the role cache is the
 * complete role list of an available guild). `mutate(prunedIds)` returns `{ ok: false, code, data? }` or `{ ok: true, roles }`.
 */
export async function updateManagers(panelId, guild, mutate) {
  try {
    return await prisma.$transaction(async (tx) => {
      const [lock] = await tx.$queryRaw(panelRowLockSql(panelId, guild.id));
      if (!lock) return { ok: false, code: 'PANEL_NOT_FOUND' };
      const row = await tx.ticketPanel.findUnique({ where: { id: panelId }, select: { managerRoleIds: true } });
      const pruned = guild.available === false ? row.managerRoleIds : row.managerRoleIds.filter((id) => guild.roles.cache.has(id));
      const res = mutate(pruned);
      if (!res.ok) return res;
      await tx.ticketPanel.update({ where: { id: panelId }, data: { managerRoleIds: { set: res.roles } } });
      return { ok: true, roles: res.roles };
    }, { timeout: 10_000 });
  } finally {
    invalidatePanel(panelId);
  }
}

/**
 * Back-fills a manager role change into the panel's existing ticket channels (newest 50, sequentially, 60 s).
 * @returns {Promise<{ updated: number, failed: number, skipped: number }>}
 */
export async function syncManagerOverwrites({ guild, panelId, roleId, mode }) {
  const where = { guildId: guild.id, panelId, status: { in: ['OPEN', 'CLAIMED', 'CLOSED'] } };
  const [total, tickets] = await Promise.all([
    prisma.ticket.count({ where }),
    prisma.ticket.findMany({ where, orderBy: { createdAt: 'desc' }, take: MANAGER_SYNC_MAX, select: { id: true, channelId: true } }),
  ]);
  let updated = 0;
  let failed = 0;
  let skipped = Math.max(0, total - tickets.length);
  const deadline = Date.now() + MANAGER_SYNC_DEADLINE_MS;
  await mapLimit(tickets, 1, async (t) => {
    if (Date.now() > deadline) { skipped++; return; }
    const channel = guild.channels.cache.get(t.channelId);
    if (!channel) { skipped++; return; }
    try {
      if (mode === 'add') {
        await channel.permissionOverwrites.edit(roleId, { ...STAFF_ALLOW_OBJ }, { type: OverwriteType.Role, reason: 'Ticket manager role added' });
      } else {
        await channel.permissionOverwrites.delete(roleId, 'Ticket manager role removed');
      }
      updated++;
    } catch (err) {
      if (mode === 'remove' && err?.code === 10009) { updated++; return; }
      failed++;
      logger.debug(`tickets: manager overwrite sync failed in ${t.channelId}: ${err?.message}`);
    }
  });
  return { updated, failed, skipped };
}

/**
 * Marks tickets whose channel Discord confirmed is gone, and removes abandoned placeholders. Never throws.
 * Cache misses are confirmed with a REST fetch first (at most RECONCILE_FETCH_MAX per call).
 * @returns {Promise<{ deleted: number, pendingRemoved: number }>}
 */
export async function reconcileGuildTickets(guild) {
  const out = { deleted: 0, pendingRemoved: 0 };
  try {
    if (guild.available === false) return out;
    const rows = await prisma.ticket.findMany({
      where: { guildId: guild.id, status: { not: 'DELETED' } },
      select: { id: true, channelId: true, createdAt: true },
      take: 1000,
      orderBy: { createdAt: 'desc' },
    });
    const { stalePending, staleGone } = classifyTicketRows(rows, (id) => guild.channels.cache.has(id), Date.now());
    if (stalePending.length) out.pendingRemoved = (await prisma.ticket.deleteMany({ where: { id: { in: stalePending } } })).count;
    if (staleGone.length) {
      const byId = new Map(rows.map((r) => [r.id, r.channelId]));
      const candidates = staleGone.slice(0, RECONCILE_FETCH_MAX);
      const gone = await confirmGoneChannels(guild, candidates.map((id) => byId.get(id)));
      const ids = candidates.filter((id) => gone.has(byId.get(id)));
      if (ids.length) {
        out.deleted = (await prisma.ticket.updateMany({ where: { id: { in: ids }, status: { not: 'DELETED' } }, data: { status: 'DELETED' } })).count;
      }
    }
  } catch (err) {
    logger.warn('tickets: reconcile failed', err);
  }
  return out;
}

/**
 * The panel list view: reconciles first, then returns each panel with its ticket count.
 * @returns {Promise<Array<{ panel: object, active: number }>>}
 */
export async function listPanelsView(guild) {
  await reconcileGuildTickets(guild);
  const [panels, counts] = await Promise.all([
    prisma.ticketPanel.findMany({ where: { guildId: guild.id }, orderBy: { name: 'asc' }, include: { options: { select: { id: true } } } }),
    prisma.ticket.groupBy({ by: ['panelId'], where: { guildId: guild.id, status: { in: ['OPEN', 'CLAIMED', 'CLOSED'] } }, _count: { _all: true } }),
  ]);
  const byPanel = new Map(counts.map((c) => [c.panelId, c._count._all]));
  return panels.slice(0, LIMITS.TICKET_PANELS).map((panel) => ({ panel, active: byPanel.get(panel.id) ?? 0 }));
}

/** One line of `/ticket-panel list`. */
export function panelListLine({ panel, active }, guild) {
  const published = panel.channelId && panel.messageId ? `<#${panel.channelId}>` : 'not published';
  const managers = liveManagerRoleIds(panel, guild).length;
  return `**${clampText(panel.name, 60)}** — ${styleName(panel)} · ${panel.options.length} option(s) · ${managers} manager role(s) · ${active} active ticket(s) · ${published}`;
}

// ── Intro and notices (§6.7.5, §6.8) ──────────────────────────────────────────

/**
 * Posts the intro message with Close / Claim in a new ticket. Pings the opener, and the manager roles only when the template
 * contains {managers} (and the bot may mention them in the channel). Never throws.
 * @returns {Promise<boolean>} whether the message was sent
 */
export async function postIntro({ channel, ticket, panel, option, member, settings }) {
  try {
    const guild = channel.guild;
    const tpl = normalizeTemplate(option?.introTemplate ?? panel?.introTemplate ?? DEFAULT_INTRO_TEMPLATE);
    const managers = liveManagerRoleIds(panel, guild);
    const vars = {
      number: formatTicketNumber(ticket.number),
      reason: ticket.reason ?? '',
      topic: option?.label ?? '',
      panel: panel?.name ?? '',
      managers: managers.map((id) => `<@&${id}>`).join(' '),
    };
    const roles = templateUsesToken(tpl, 'managers') && managers.length ? pingableRoleIds(channel, guild.members?.me, managers).allowed : [];
    const opts = {
      defaultColor: settings?.embedColor ?? COLORS.INFO,
      allowedMentions: { parse: [], users: [ticket.ownerId], roles },
      extraComponents: [buildIntroRow(ticket.id, { claimed: false })],
    };
    const ctx = { member, user: member?.user, guild, channel, vars };
    const payload = renderTemplate(tpl, ctx, opts) ?? renderTemplate(DEFAULT_INTRO_TEMPLATE, ctx, opts);

    if (ticket.reason && !templateUsesToken(tpl, 'reason')) {
      let added = false;
      const embed = payload.embeds[0];
      if (embed && (embed.data.fields?.length ?? 0) < 25) {
        const budget = 6000 - embedLength(embed.data) - 'Reason'.length;
        if (budget >= 20) {
          embed.addFields({ name: 'Reason', value: clampText(ticket.reason, Math.min(1024, budget)) });
          added = true;
        }
      }
      if (!added) payload.content = clampText(`${payload.content ?? ''}\n\n**Reason:** ${ticket.reason}`.trim(), 2000);
    }
    await channel.send(payload);
    return true;
  } catch (err) {
    logger.warn('tickets: could not post the intro message', err);
    return false;
  }
}

/** Sends a public notice in a ticket channel. Never throws. @returns {Promise<boolean>} */
export async function postNotice(channel, payload) {
  try {
    await channel.send(payload);
    return true;
  } catch (err) {
    logger.debug(`tickets: notice failed in ${channel?.id}: ${err?.message}`);
    return false;
  }
}

/**
 * The public notice for a lifecycle event.
 * @param {'claim'|'close'|'reopen'|'add'|'remove'|'rename'|'delete'} kind
 * @param {object} d `{ actorId, ticketId, ... }` plus per-kind fields
 * @param {object|null} settings
 */
export function buildNotice(kind, d, settings) {
  const mentions = { parse: [] };
  switch (kind) {
    case 'claim':
      return {
        embeds: [guildEmbed(settings, '🙋 Ticket Claimed', d.previousId
          ? `<@${d.actorId}> took over this ticket from <@${d.previousId}>.`
          : `<@${d.actorId}> is handling this ticket.`)],
        allowedMentions: mentions,
      };
    case 'close': {
      const embed = guildEmbed(settings, '🔒 Ticket Closed', `Closed by <@${d.actorId}>.`);
      if (d.reason) embed.addFields({ name: 'Reason', value: clampText(d.reason, 1024) });
      return { embeds: [embed], components: [buildClosedRow(d.ticketId)], allowedMentions: mentions };
    }
    case 'reopen':
      return {
        embeds: [guildEmbed(settings, '🔓 Ticket Reopened', `Reopened by <@${d.actorId}>.`)],
        components: [buildIntroRow(d.ticketId, { claimed: Boolean(d.claimed) })],
        allowedMentions: mentions,
      };
    case 'add':
      return { embeds: [guildEmbed(settings, '➕ Member Added', `<@${d.userId}> was added by <@${d.actorId}>.`)], allowedMentions: { parse: [], users: [d.userId] } };
    case 'remove':
      return { embeds: [guildEmbed(settings, '➖ Member Removed', `<@${d.userId}> was removed by <@${d.actorId}>.`)], allowedMentions: mentions };
    case 'rename':
      return { embeds: [guildEmbed(settings, '✏️ Ticket Renamed', `Renamed to <#${d.channelId}> by <@${d.actorId}>.`)], allowedMentions: mentions };
    case 'delete':
      return { embeds: [guildEmbed(settings, '🗑️ Deleting Ticket', deleteNoticeText(d))], allowedMentions: mentions };
    default:
      return { embeds: [guildEmbed(settings, 'Ticket', '')], allowedMentions: mentions };
  }
}

/** The description of the delete notice: who deleted it, and where the transcript went. */
export function deleteNoticeText(d) {
  const lines = [`Deleted by <@${d.actorId}>. This channel will be deleted in 5 seconds.`];
  if (d.transcriptChannelId) lines.push(`The transcript was saved in <#${d.transcriptChannelId}>.`);
  if (d.dm) lines.push('A copy was sent to the opener by DM.');
  return lines.join('\n');
}

// ── Transcripts (§6.10) ───────────────────────────────────────────────────────

/** @returns {string} the text with & < > " ' escaped */
export function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

/** @returns {string} '512 B', '1.2 KB', '3.4 MB' (base 1024, one decimal) */
export function formatBytes(n) {
  const v = Math.max(0, Number(n) || 0);
  if (v < 1024) return `${Math.floor(v)} B`;
  if (v < 1024 ** 2) return `${(v / 1024).toFixed(1)} KB`;
  if (v < 1024 ** 3) return `${(v / 1024 ** 2).toFixed(1)} MB`;
  return `${(v / 1024 ** 3).toFixed(1)} GB`;
}

const SYSTEM_LABELS = Object.freeze({
  [MessageType.ChannelPinnedMessage]: 'pinned a message',
  [MessageType.UserJoin]: 'joined the server',
  [MessageType.ThreadCreated]: 'started a thread',
  [MessageType.RecipientAdd]: 'added someone',
  [MessageType.RecipientRemove]: 'removed someone',
  [MessageType.ChannelNameChange]: 'changed the channel name',
  [MessageType.ChannelIconChange]: 'changed the channel icon',
});

const clampField = (s, max) => clampText(s ?? '', max);

/**
 * One message as a plain transcript entry. Mentions and emoji are resolved to text in a single pass, so a substituted name is
 * never expanded again.
 * @param {import('discord.js').Message} message
 * @param {import('discord.js').Guild} guild
 */
export function normalizeMessage(message, guild) {
  const author = message.author ?? {};
  const resolved = String(message.content ?? '').replace(
    /<@!?(\d+)>|<@&(\d+)>|<#(\d+)>|<a?:(\w+):\d+>|<t:(-?\d+)(?::[tTdDfFR])?>/g,
    (whole, user, role, chan, emoji, stamp) => {
      if (user) {
        return `@${message.mentions?.members?.get(user)?.displayName ?? message.mentions?.users?.get(user)?.username
          ?? guild?.members?.cache?.get(user)?.displayName ?? user}`;
      }
      if (role) return `@${guild?.roles?.cache?.get(role)?.name ?? 'deleted-role'}`;
      if (chan) return `#${guild?.channels?.cache?.get(chan)?.name ?? 'deleted-channel'}`;
      if (emoji) return `:${emoji}:`;
      const d = new Date(Number(stamp) * 1000);
      return Number.isNaN(d.getTime()) ? whole : d.toISOString();
    },
  );
  const embeds = Array.isArray(message.embeds) ? message.embeds : valuesOf(message.embeds);
  return {
    id: message.id,
    authorId: author.id ?? '',
    authorName: message.member?.displayName ?? author.globalName ?? author.username ?? 'Unknown',
    authorTag: author.tag ?? author.username ?? '',
    bot: Boolean(author.bot),
    createdTimestamp: message.createdTimestamp ?? null,
    editedTimestamp: message.editedTimestamp ?? null,
    system: Boolean(message.system),
    systemLabel: message.system ? (SYSTEM_LABELS[message.type] ?? 'system message') : '',
    content: clampText(resolved, 4000),
    replyToId: message.reference?.messageId ?? null,
    attachments: valuesOf(message.attachments)
      .filter((a) => isHttpUrl(a.url))
      .map((a) => ({ name: clampField(a.name, 200), url: a.url, size: a.size ?? 0 })),
    embeds: embeds.map((e) => ({
      title: clampField(e.title, 256),
      description: clampField(e.description, 4096),
      fields: (e.fields ?? []).slice(0, 25).map((f) => ({ name: clampField(f.name, 256), value: clampField(f.value, 1024) })),
      footer: clampField(e.footer?.text, 2048),
    })),
    stickers: valuesOf(message.stickers).map((s) => clampField(s.name, 100)),
  };
}

const TRANSCRIPT_CSS = 'body{background:#1e1f22;color:#dbdee1;font:14px/1.4 sans-serif;margin:0;padding:16px}h1{font-size:20px}'
  + 'dl{display:grid;grid-template-columns:max-content 1fr;gap:2px 12px;color:#949ba4}dd{margin:0;color:#dbdee1}'
  + '.msg{padding:6px 0;border-top:1px solid #2b2d31}.meta{color:#949ba4;font-size:12px}.author{color:#fff;font-weight:600;font-size:14px}'
  + '.content{white-space:pre-wrap;word-wrap:break-word}.badge{background:#5865f2;color:#fff;border-radius:3px;padding:0 4px;font-size:10px}'
  + '.embed{border-left:4px solid #5865f2;background:#2b2d31;padding:6px 10px;margin:4px 0;white-space:pre-wrap}.et{font-weight:600}'
  + '.sys,.att,.reply,.note{color:#949ba4;font-size:13px}a{color:#00a8fc}';

/**
 * A self-contained HTML transcript: no scripts, images or remote loads (a CSP meta blocks them too), every dynamic value
 * escaped. When the file would exceed `maxBytes` the oldest messages are dropped and the header says so.
 * @param {object} meta ticket and server details (see buildTranscript)
 * @param {Array<object>} entries chronological entries from normalizeMessage
 * @param {{ timeZone?: string, maxBytes?: number }} [opts]
 * @returns {{ html: string, included: number, omitted: number }}
 */
export function renderTranscriptHtml(meta, entries, { timeZone = 'UTC', maxBytes = MAX_TRANSCRIPT_BYTES } = {}) {
  const tz = isValidTimeZone(timeZone) ? timeZone : 'UTC';
  let formatter = null;
  try {
    formatter = new Intl.DateTimeFormat('en-GB', { timeZone: tz, dateStyle: 'medium', timeStyle: 'short' });
  } catch {
    formatter = null;
  }
  const fmtTime = (ms) => {
    if (ms === null || ms === undefined || !Number.isFinite(Number(ms))) return '—';
    try {
      return formatter ? formatter.format(Number(ms)) : new Date(Number(ms)).toISOString();
    } catch {
      return '—';
    }
  };
  const isoOf = (ms) => {
    const d = new Date(Number(ms));
    return Number.isNaN(d.getTime()) ? '' : d.toISOString();
  };
  const esc = escapeHtml;
  const num = esc(meta.num);

  const article = (e) => {
    let h = `<article class="msg${e.bot ? ' bot' : ''}${e.system ? ' sys' : ''}" id="m-${esc(e.id)}">`
      + `<div class="meta"><span class="author">${esc(e.authorName)}</span> <span class="tag">${esc(e.authorTag)} · ${esc(e.authorId)}</span>`
      + `${e.bot ? ' <span class="badge">BOT</span>' : ''} <time datetime="${esc(isoOf(e.createdTimestamp))}">${esc(fmtTime(e.createdTimestamp))}</time>`
      + `${e.editedTimestamp ? ` <span class="edited">(edited ${esc(fmtTime(e.editedTimestamp))})</span>` : ''}</div>`;
    if (e.replyToId) h += `<div class="reply">↪ <a href="#m-${esc(e.replyToId)}">reply</a></div>`;
    if (e.system) h += `<div class="sys">${esc(e.systemLabel || 'system message')}</div>`;
    if (e.content) h += `<div class="content">${esc(e.content)}</div>`;
    for (const em of e.embeds ?? []) {
      h += '<div class="embed">';
      if (em.title) h += `<div class="et">${esc(em.title)}</div>`;
      if (em.description) h += `<div class="ed">${esc(em.description)}</div>`;
      for (const f of em.fields ?? []) h += `<div class="ef"><b>${esc(f.name)}</b><div>${esc(f.value)}</div></div>`;
      if (em.footer) h += `<div class="eft">${esc(em.footer)}</div>`;
      h += '</div>';
    }
    for (const a of e.attachments ?? []) {
      const size = a.size ? ` (${formatBytes(a.size)})` : '';
      h += isHttpUrl(a.url)
        ? `<div class="att">📎 <a href="${esc(a.url)}" rel="noopener noreferrer nofollow">${esc(a.name)}</a>${size}</div>`
        : `<div class="att">📎 ${esc(a.name)}${size}</div>`;
    }
    for (const s of e.stickers ?? []) h += `<div class="att">🏷️ Sticker: ${esc(s)}</div>`;
    return `${h}</article>\n`;
  };

  const head = (omitted) => `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Ticket #${num} · ${esc(meta.guildName)} — transcript</title>
<style>${TRANSCRIPT_CSS}</style>
</head><body>
<header><h1>Ticket #${num}</h1><dl>
<dt>Server</dt><dd>${esc(meta.guildName)}</dd><dt>Channel</dt><dd>#${esc(meta.channelName)}</dd><dt>Panel</dt><dd>${esc(meta.panelName || '—')}</dd>
<dt>Opened by</dt><dd>${esc(meta.ownerTag)} (${esc(meta.ownerId)})</dd><dt>Opened</dt><dd>${esc(fmtTime(meta.openedAt))}</dd>
<dt>Claimed by</dt><dd>${esc(meta.claimedTag || '—')}</dd><dt>Closed</dt><dd>${esc(meta.closedAt ? fmtTime(meta.closedAt) : '—')}</dd><dt>Reason</dt><dd>${esc(meta.reason || '—')}</dd>
<dt>Times</dt><dd>${esc(tz)}</dd>
</dl>${omitted ? `<p class="note">${omitted} earlier messages are not included (file size limit).</p>` : ''}${meta.truncated === 'limit' ? `<p class="note">Only the last ${MAX_TRANSCRIPT_MESSAGES} messages are included.</p>` : ''}${meta.truncated === 'time' ? '<p class="note">The transcript stopped early (time limit); older messages are missing.</p>' : ''}
</header><main>
`;
  const foot = (included) => `</main><footer>Generated ${esc(fmtTime(meta.generatedAt))} by ${esc(meta.generatedByTag)} · ${included} message(s) · Attachment links point to Discord and may stop working.</footer>
</body></html>
`;

  const list = entries ?? [];
  const articles = list.map(article);
  const fixed = Buffer.byteLength(head(list.length)) + Buffer.byteLength(foot(list.length)) + 512;
  let sum = 0;
  let start = list.length;
  for (let i = list.length - 1; i >= 0; i--) {
    const b = Buffer.byteLength(articles[i]);
    if (fixed + sum + b > maxBytes) break;
    sum += b;
    start = i;
  }
  const included = list.length - start;
  const omitted = start;
  return { html: head(omitted) + articles.slice(start).join('') + foot(included), included, omitted };
}

/**
 * Reads the channel history (100 per request, newest first) and returns it chronologically.
 * @returns {Promise<{ messages: import('discord.js').Message[], truncated: null|'limit'|'time' }>}
 * @throws the Discord error when a page cannot be read
 */
export async function fetchTranscriptMessages(channel, { max = MAX_TRANSCRIPT_MESSAGES, deadlineMs = TRANSCRIPT_DEADLINE_MS } = {}) {
  const out = [];
  let before;
  let truncated = null;
  const start = Date.now();
  for (;;) {
    const batch = await channel.messages.fetch({ limit: TRANSCRIPT_PAGE_SIZE, before, cache: false });
    out.push(...batch.values());
    const full = batch.size >= TRANSCRIPT_PAGE_SIZE;
    if (out.length > max || (out.length === max && full)) { truncated = 'limit'; break; }
    if (!full) break;
    if (Date.now() - start > deadlineMs) { truncated = 'time'; break; }
    before = batch.last().id;
  }
  out.splice(max);
  return { messages: out.reverse(), truncated };
}

/**
 * Builds the transcript file and its summary embed.
 * @returns {Promise<{ ok: true, file: AttachmentBuilder, dmFile: AttachmentBuilder, embed: import('discord.js').EmbedBuilder,
 *   count: number, truncated: string|null, omitted: number } | { ok: false, reason: string }>}
 */
export async function buildTranscript({ guild, ticket, channel, settings, generatedBy, deletedBy = null }) {
  try {
    const missing = missingBotPermissions(channel, ['ViewChannel', 'ReadMessageHistory']);
    if (missing.length) return { ok: false, reason: `missing ${permissionNames(missing).join(', ')}` };

    const { messages, truncated } = await fetchTranscriptMessages(channel);
    const entries = messages.map((m) => normalizeMessage(m, guild));

    const owner = await withTimeout(guild.client.users.fetch(ticket.ownerId), 3000, 'transcript owner').catch(() => null);
    const claimed = ticket.claimedById ? guild.members.cache.get(ticket.claimedById)?.user : null;
    const num = formatTicketNumber(ticket.number);
    const timeZone = isValidTimeZone(settings?.timezone) ? settings.timezone : 'UTC';
    const meta = {
      num,
      guildName: guild.name,
      channelName: channel.name,
      panelName: ticket.panel?.name ?? '',
      ownerTag: owner?.tag ?? ticket.ownerId,
      ownerId: ticket.ownerId,
      openedAt: ticket.createdAt instanceof Date ? ticket.createdAt.getTime() : Number(ticket.createdAt),
      claimedTag: ticket.claimedById ? (claimed?.tag ?? ticket.claimedById) : '',
      closedAt: ticket.closedAt instanceof Date ? ticket.closedAt.getTime() : null,
      reason: ticket.reason ?? '',
      truncated,
      generatedAt: Date.now(),
      generatedByTag: generatedBy?.tag ?? generatedBy?.username ?? 'the bot',
    };
    const { html, included, omitted } = renderTranscriptHtml(meta, entries, { timeZone });
    const buf = Buffer.from(html, 'utf8');
    const name = `transcript-ticket-${num}.html`;
    const description = `Ticket #${num} transcript`;

    const embed = guildEmbed(settings, `📄 Ticket #${num} transcript`).addFields(
      { name: 'Opened by', value: `<@${ticket.ownerId}>`, inline: true },
      { name: 'Claimed by', value: ticket.claimedById ? `<@${ticket.claimedById}>` : '—', inline: true },
      { name: 'Closed by', value: ticket.closedById ? `<@${ticket.closedById}>` : '—', inline: true },
      ...(deletedBy ? [{ name: 'Deleted by', value: `<@${deletedBy.id}>`, inline: true }] : []),
      { name: 'Panel', value: clampText(ticket.panel?.name || '—', 1024), inline: true },
      { name: 'Opened', value: discordTimestamp(ticket.createdAt, 'f'), inline: true },
      { name: 'Closed', value: ticket.closedAt ? discordTimestamp(ticket.closedAt, 'f') : '—', inline: true },
      { name: 'Messages', value: `${formatNumber(included)}${omitted ? ` (+${formatNumber(omitted)} omitted)` : ''}`, inline: true },
      ...(ticket.reason ? [{ name: 'Reason', value: clampText(ticket.reason, 1024), inline: false }] : []),
    );
    return {
      ok: true,
      file: new AttachmentBuilder(buf, { name, description }),
      dmFile: new AttachmentBuilder(buf, { name, description }),
      embed,
      count: included,
      truncated,
      omitted,
    };
  } catch (err) {
    logger.warn('tickets: building a transcript failed', err);
    return { ok: false, reason: describeDiscordError(err) };
  }
}

/** Throttled on-demand transcript (`/ticket transcript`, the Transcript button): one per ticket per minute. */
export function allowTranscript(ticketId) {
  return transcriptThrottle(ticketId);
}

// ── Opening a ticket (§6.7) ───────────────────────────────────────────────────

/**
 * The member's live tickets on a panel. Read-only and cache-based: a ticket whose channel is not in the cache does not count
 * here (so a member is never blocked by a guess); the authoritative check is inside openTicket.
 */
export async function listLiveTickets(guild, guildId, panelId, ownerId) {
  const rows = await prisma.ticket.findMany({
    where: { guildId, panelId, ownerId, status: { in: ['OPEN', 'CLAIMED'] } },
    select: { id: true, channelId: true, createdAt: true },
    orderBy: { createdAt: 'asc' },
  });
  return classifyTicketRows(rows, (id) => guild.channels.cache.has(id), Date.now()).live;
}

const deleteRow = (id) => prisma.ticket.deleteMany({ where: { id } }).catch((err) => logger.warn('tickets: could not remove a reserved ticket row', err));

/**
 * Opens a ticket: reserves a numbered row under an advisory lock, creates the private channel and posts the intro.
 * @returns {Promise<{ ok: true, ticket: object, channel: object, notes: string[] } | { ok: false, code: string, data?: object }>}
 */
export async function openTicket({ interaction, client, guild, panel, option, reason = null, settings }) {
  const userId = interaction.user.id;
  if (!openThrottle(`${guild.id}:${userId}`)) return { ok: false, code: 'THROTTLED' };

  const missing = missingBotPermissions(guild, OPEN_REQUIRED_PERMS);
  if (missing.length) return { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(missing).join(', '), where: null } };

  // Reserve the number and the row. The cap check and the insert share one per-guild advisory lock, so parallel opens cannot
  // exceed maxOpenPerUser or reuse a number; there is no Discord call inside the transaction.
  let res;
  try {
    await ensureGuildSettings(guild.id);
    const liveWhere = { guildId: guild.id, panelId: panel.id, ownerId: userId, status: { in: ['OPEN', 'CLAIMED'] } };
    const pre = await prisma.ticket.findMany({ where: liveWhere, select: { channelId: true } });
    // Rule 20: only a channel Discord confirmed is gone (10003) may be written off; everything else counts as live.
    const gone = await confirmGoneChannels(guild, pre.map((r) => r.channelId).filter((id) => !id.startsWith(PENDING_PREFIX)));
    res = await prisma.$transaction(async (tx) => {
      await tx.$queryRaw(numberLockSql(guild.id));
      const rows = await tx.ticket.findMany({ where: liveWhere, select: { id: true, channelId: true, createdAt: true } });
      const { live, stalePending, staleGone } = classifyTicketRows(rows, (id) => !gone.has(id), Date.now());
      if (stalePending.length) await tx.ticket.deleteMany({ where: { id: { in: stalePending } } });
      if (staleGone.length) await tx.ticket.updateMany({ where: { id: { in: staleGone } }, data: { status: 'DELETED' } });
      if (live.length >= panel.maxOpenPerUser) return { limit: live };
      const [{ next }] = await tx.$queryRaw(nextNumberSql(guild.id));
      const ticket = await tx.ticket.create({
        data: {
          guildId: guild.id, panelId: panel.id, optionId: option?.id ?? null, number: next,
          channelId: PENDING_PREFIX + interaction.id, ownerId: userId, reason,
        },
      });
      return { ticket };
    }, { timeout: 10_000, maxWait: 5_000 });
  } catch (err) {
    logger.error('tickets: reserving a ticket failed', err);
    return { ok: false, code: 'DB_ERROR' };
  }
  if (res.limit) return { ok: false, code: 'OPEN_LIMIT', data: { ids: res.limit.map((r) => r.channelId), max: panel.maxOpenPerUser } };
  const { ticket } = res;

  const parent = await resolveOpenParent(guild, panel, option);
  if (parent.missing) {
    await deleteRow(ticket.id);
    return { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(parent.missing).join(', '), where: parent.category.name } };
  }
  const notes = [];
  if (parent.note) notes.push(parent.note);

  const num = formatTicketNumber(ticket.number);
  const createOptions = (parentId) => ({
    name: renderChannelName(panel.channelNameFormat, { number: ticket.number, user: interaction.user, member: interaction.member, topic: option?.label }),
    type: ChannelType.GuildText,
    parent: parentId,
    topic: clampText(`Ticket #${num} · opened by ${interaction.user.tag} (${userId})${option?.label ? ` · ${option.label}` : ''}`, 1024),
    permissionOverwrites: buildTicketOverwrites({
      guildId: guild.id, botId: client.user.id, ownerId: userId, managerRoleIds: liveManagerRoleIds(panel, guild), addedUserIds: [], closed: false,
    }),
    reason: clampText(`Ticket #${num} opened by ${interaction.user.tag}`, 512),
  });

  let channel = null;
  let failure = null;
  try {
    channel = await guild.channels.create(createOptions(parent.category?.id ?? null));
  } catch (err) {
    failure = err;
    if (isCategoryFullError(err) && parent.category) {
      notes.push(FULL_NOTE);
      try {
        channel = await guild.channels.create(createOptions(null));
        failure = null;
      } catch (err2) {
        failure = err2;
      }
    }
  }
  if (!channel) {
    await deleteRow(ticket.id);
    logger.warn(`tickets: channel create failed in guild ${guild.id}: ${failure?.message}`);
    if (failure?.code === 30013) return { ok: false, code: 'GUILD_FULL' };
    if (failure?.code === 50013 || failure?.code === 50001) {
      return { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(['ManageChannels', 'ManageRoles']).join(', '), where: null } };
    }
    return { ok: false, code: 'CREATE_FAILED', data: { reason: describeDiscordError(failure) } };
  }

  try {
    await prisma.ticket.update({ where: { id: ticket.id }, data: { channelId: channel.id } });
  } catch (err) {
    logger.error('tickets: attaching the channel to its ticket failed', err);
    await channel.delete('Ticket could not be saved').catch(() => {});
    await deleteRow(ticket.id);
    return { ok: false, code: 'DB_ERROR' };
  }

  const introOk = await postIntro({
    channel, ticket, panel, option, member: interaction.member, settings: settings === undefined ? await loadSettings(guild.id) : settings,
  });
  if (!introOk) notes.push("I couldn't post the welcome message in the ticket.");
  return { ok: true, ticket: { ...ticket, channelId: channel.id }, channel, notes };
}

// ── Lifecycle actions (§6.8) ──────────────────────────────────────────────────

const reasonFor = (action, actor, t) => clampText(`Ticket #${formatTicketNumber(t.number)}: ${action} by ${actor.user?.tag ?? actor.id}`, 512);

// Steps 1-4 of the skeleton: re-read, who, state, channel and the bot's permissions. Nothing is changed.
async function prepareAction(action, { guild, ticket, actor, settings, source }) {
  const t = await getTicketById(guild.id, ticket.id);
  if (!t || t.status === 'DELETED') return { ok: false, code: 'TICKET_DELETED' };
  if (deleting.has(t.id)) return { ok: false, code: 'DELETING' };

  const isOwner = actor.id === t.ownerId;
  const isStaff = isTicketStaff(actor, t.panel, settings);
  const can = canPerform(action, { isOwner, isStaff });
  if (!can.ok) return can;

  const isAdmin = resolvePermissionLevel(actor, settings) >= PERMISSION_LEVELS.ADMIN;
  const tr = transitionFor(action, t, { actorId: actor.id, isAdmin, source });
  if (!tr.ok) return tr;

  const found = await resolveChannel(guild, t.channelId);
  if (found.state === 'gone') {
    await prisma.ticket.updateMany({ where: { id: t.id, status: { not: 'DELETED' } }, data: { status: 'DELETED' } });
    return { ok: false, code: 'TICKET_DELETED' };
  }
  if (!found.channel) return { ok: false, code: 'UNKNOWN' };
  const perms = ACTION_PERMS[action];
  if (perms) {
    const lacking = missingBotPermissions(found.channel, perms);
    if (lacking.length) return { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(lacking).join(', '), channelId: found.channel.id } };
  }
  return { ok: true, t, channel: found.channel, tr, isOwner, isStaff, isAdmin };
}

// Runs an action inside the ticket's mutex; an unexpected error is logged and becomes UNKNOWN.
function runAction(ctx, action, body) {
  return ticketMutex.run(ctx.ticket.id, async () => {
    try {
      const p = await prepareAction(action, ctx);
      if (!p.ok) return p;
      return await body(p);
    } catch (err) {
      logger.error(`tickets: ${action} failed`, err);
      return { ok: false, code: 'UNKNOWN' };
    }
  });
}

// Conditional status write: a concurrent change (count 0) is a CONFLICT.
async function writeStatus(t, data, extraWhere = {}) {
  const r = await prisma.ticket.updateMany({ where: { id: t.id, status: t.status, ...extraWhere }, data });
  return r.count > 0;
}

// Edits the opener's and the added members' overwrites; one aggregated note when any edit fails.
async function editMemberAccess(channel, t, perms, reason) {
  let failed = 0;
  for (const id of [t.ownerId, ...t.addedUserIds]) {
    try {
      await channel.permissionOverwrites.edit(id, perms, { type: OverwriteType.Member, reason });
    } catch (err) {
      failed++;
      logger.debug(`tickets: overwrite edit failed in ${channel.id}: ${err?.message}`);
    }
  }
  return failed ? `I couldn't update the permissions of ${failed} member(s) in the ticket.` : null;
}

async function finish(channel, payload, notes) {
  if (!(await postNotice(channel, payload))) notes.push("I couldn't post the notice in the ticket.");
  return { ok: true, notes };
}

/** Claim (or, for admins, take over) a ticket. @returns {Promise<{ ok: true, notes: string[] } | { ok: false, code: string, data?: object }>} */
export function claimTicket(ctx) {
  return runAction(ctx, 'claim', async ({ t, channel, tr }) => {
    const notes = [];
    const changed = await writeStatus(t, { status: 'CLAIMED', claimedById: ctx.actor.id }, tr.takeover ? { claimedById: t.claimedById } : {});
    if (!changed) return { ok: false, code: 'CONFLICT' };
    const move = await moveChannel(channel, await resolveKindParent(ctx.guild, t.panelId, 'claimed'), reasonFor('claim', ctx.actor, t));
    if (move.note) notes.push(move.note);
    return finish(channel, buildNotice('claim', { actorId: ctx.actor.id, previousId: tr.takeover ? t.claimedById : null }, ctx.settings), notes);
  });
}

/** Close a ticket: the opener and added members can read but not write. `reason` is shown in the notice only. */
export function closeTicket(ctx) {
  return runAction(ctx, 'close', async ({ t, channel }) => {
    const notes = [];
    const changed = await writeStatus(t, { status: 'CLOSED', closedById: ctx.actor.id, closedAt: new Date() });
    if (!changed) return { ok: false, code: 'CONFLICT' };
    const why = reasonFor('close', ctx.actor, t);
    const note = await editMemberAccess(channel, t, { ViewChannel: true, SendMessages: false, ReadMessageHistory: true }, why);
    if (note) notes.push(note);
    const move = await moveChannel(channel, await resolveKindParent(ctx.guild, t.panelId, 'closed'), why);
    if (move.note) notes.push(move.note);
    return finish(channel, buildNotice('close', { actorId: ctx.actor.id, ticketId: t.id, reason: ctx.reason ?? null }, ctx.settings), notes);
  });
}

/** Reopen a closed ticket: back to CLAIMED when it had been claimed, else OPEN. */
export function reopenTicket(ctx) {
  return runAction(ctx, 'reopen', async ({ t, channel, tr }) => {
    const notes = [];
    const changed = await writeStatus(t, { status: tr.to, closedById: null, closedAt: null });
    if (!changed) return { ok: false, code: 'CONFLICT' };
    const why = reasonFor('reopen', ctx.actor, t);
    const note = await editMemberAccess(channel, t, { ViewChannel: true, SendMessages: true, ReadMessageHistory: true }, why);
    if (note) notes.push(note);
    const parent = tr.to === 'CLAIMED'
      ? await resolveKindParent(ctx.guild, t.panelId, 'claimed')
      : await resolveOpenParent(ctx.guild, t.panel, t.option, { fullNote: (c) => `**${c.name}** is full (50 channels), so I left the ticket where it is.` });
    const move = await moveChannel(channel, parent, why);
    if (move.note) notes.push(move.note);
    return finish(channel, buildNotice('reopen', { actorId: ctx.actor.id, ticketId: t.id, claimed: tr.to === 'CLAIMED' }, ctx.settings), notes);
  });
}

/** Give a member access to the ticket. `user` is the resolved User, `member` the GuildMember or null. */
export function addMember(ctx) {
  const { user, member } = ctx;
  return runAction(ctx, 'add', async ({ t, channel }) => {
    if (!member) return { ok: false, code: 'NOT_MEMBER' };
    if (user.bot) return { ok: false, code: 'ADD_BOT' };
    if (user.id === t.ownerId) return { ok: false, code: 'IS_OWNER', data: { op: 'add' } };
    if (t.addedUserIds.includes(user.id)) return { ok: false, code: 'ALREADY_ADDED', data: { userId: user.id } };
    if (t.addedUserIds.length >= MAX_ADDED_USERS) return { ok: false, code: 'ADDED_LIMIT' };
    try {
      await channel.permissionOverwrites.edit(user.id, {
        ViewChannel: true, SendMessages: t.status !== 'CLOSED', ReadMessageHistory: true, AttachFiles: true, EmbedLinks: true,
      }, { type: OverwriteType.Member, reason: reasonFor('add', ctx.actor, t) });
    } catch (err) {
      logger.debug(`tickets: add member failed in ${channel.id}: ${err?.message}`);
      return err?.code === 50013 || err?.code === 50001
        ? { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(['ManageRoles']).join(', '), channelId: channel.id } }
        : { ok: false, code: 'UNKNOWN' };
    }
    await prisma.ticket.update({ where: { id: t.id }, data: { addedUserIds: { set: [...t.addedUserIds, user.id] } } });
    return finish(channel, buildNotice('add', { actorId: ctx.actor.id, userId: user.id }, ctx.settings), []);
  });
}

/** Remove a member that was added with `add`. */
export function removeMember(ctx) {
  const { user } = ctx;
  return runAction(ctx, 'remove', async ({ t, channel }) => {
    if (user.id === t.ownerId) return { ok: false, code: 'IS_OWNER', data: { op: 'remove' } };
    if (!t.addedUserIds.includes(user.id)) return { ok: false, code: 'NOT_ADDED', data: { userId: user.id } };
    try {
      await channel.permissionOverwrites.delete(user.id, reasonFor('remove', ctx.actor, t));
    } catch (err) {
      if (err?.code !== 10009) { // 10009 Unknown Overwrite: already gone
        logger.debug(`tickets: remove member failed in ${channel.id}: ${err?.message}`);
        return err?.code === 50013 || err?.code === 50001
          ? { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(['ManageRoles']).join(', '), channelId: channel.id } }
          : { ok: false, code: 'UNKNOWN' };
      }
    }
    await prisma.ticket.update({ where: { id: t.id }, data: { addedUserIds: { set: t.addedUserIds.filter((id) => id !== user.id) } } });
    return finish(channel, buildNotice('remove', { actorId: ctx.actor.id, userId: user.id }, ctx.settings), []);
  });
}

/** Rename the channel. Discord allows 2 renames per 10 minutes, so a third is refused up front. */
export function renameTicket(ctx) {
  return runAction(ctx, 'rename', async ({ t, channel }) => {
    const notes = [];
    const clean = sanitizeChannelName(ctx.name);
    if (!clean) return { ok: false, code: 'BAD_NAME' };
    const now = Date.now();
    const w = renameWindow(channel.id, now);
    if (!w.allowed) return { ok: false, code: 'RENAME_LIMIT', data: { retryAt: w.retryAt } };
    try {
      await withTimeout(channel.setName(clean, reasonFor('rename', ctx.actor, t)), 15_000, 'ticket rename');
      recordRename(channel.id, now);
    } catch (err) {
      if (err instanceof TimeoutError) {
        recordRename(channel.id, now);
        notes.push('Discord is rate limiting renames; the new name will appear shortly.');
      } else {
        logger.debug(`tickets: rename failed in ${channel.id}: ${err?.message}`);
        return err?.code === 50013 || err?.code === 50001
          ? { ok: false, code: 'BOT_PERMS', data: { names: permissionNames(['ManageChannels']).join(', '), channelId: channel.id } }
          : { ok: false, code: 'UNKNOWN' };
      }
    }
    return finish(channel, buildNotice('rename', { actorId: ctx.actor.id, channelId: channel.id }, ctx.settings), notes);
  });
}

/**
 * Deletes a ticket: transcript first (channel and/or DM), notice, 5 s pause, then the channel. The status becomes DELETED only
 * after the channel is gone. `onSaved({ text })` is called once the transcript is safe, before the pause; the caller must not use
 * its interaction after that (the channel holding it is about to disappear).
 * @returns {Promise<{ ok: true } | { ok: false, code: string, data?: object }>}
 */
export async function deleteTicket(ctx) {
  const { guild, actor, settings, source, onSaved } = ctx;
  const client = guild.client;

  // Phase 1 inside the mutex: checks, then mark the ticket as being deleted and release the mutex. A concurrent action then gets
  // DELETING at once instead of queueing behind the transcript and the pause.
  const prep = await ticketMutex.run(ctx.ticket.id, async () => {
    try {
      const p = await prepareAction('delete', { guild, ticket: ctx.ticket, actor, settings, source });
      if (p.ok) deleting.add(p.t.id);
      return p;
    } catch (err) {
      logger.error('tickets: delete failed', err);
      return { ok: false, code: 'UNKNOWN' };
    }
  });
  if (!prep.ok) return prep;
  const { t, channel } = prep;

  try {
    const wantChannel = Boolean(t.panel?.transcriptChannelId);
    const wantDm = Boolean(t.panel?.dmTranscript);

    let tc = null;
    if (wantChannel) {
      const r = await resolveChannel(guild, t.panel.transcriptChannelId);
      if (r.channel) {
        const u = checkChannelUsable(r.channel, { needs: TRANSCRIPT_POST_PERMS });
        if (!u.ok) {
          const reason = u.missing.length ? `missing ${permissionNames(u.missing).join(', ')}` : String(u.reason ?? 'unusable').toLowerCase();
          return { ok: false, code: 'TRANSCRIPT_CHANNEL', data: { channelId: r.channel.id, reason } };
        }
        tc = r.channel;
      } else if (r.state !== 'gone') {
        return { ok: false, code: 'TRANSCRIPT_CHANNEL', data: { channelId: t.panel.transcriptChannelId, reason: "I couldn't reach it right now" } };
      } // gone: the channelDelete hook clears the setting; the delete continues without a channel copy
    }

    let dmNote = null;
    if (tc || wantDm) {
      const tr = await buildTranscript({ guild, ticket: t, channel, settings, generatedBy: actor.user, deletedBy: actor.user });
      if (!tr.ok) return { ok: false, code: 'TRANSCRIPT_ABORT', data: { reason: tr.reason } };
      if (tc) {
        try {
          await tc.send({ embeds: [tr.embed], files: [tr.file], allowedMentions: { parse: [] } });
        } catch (err) {
          return { ok: false, code: 'TRANSCRIPT_ABORT', data: { reason: describeDiscordError(err) } };
        }
      }
      if (wantDm) {
        try {
          const owner = await client.users.fetch(t.ownerId);
          await owner.send({ embeds: [tr.embed.setFooter({ text: clampText(`Sent from ${guild.name}`, 2048) })], files: [tr.dmFile] });
        } catch (err) {
          logger.debug(`tickets: transcript DM failed: ${err?.message}`);
          dmNote = 'The opener has DMs closed, so they did not get a copy.';
        }
      }
    }

    const notice = buildNotice('delete', { actorId: actor.id, transcriptChannelId: tc?.id ?? null, dm: wantDm && !dmNote }, settings);
    await postNotice(channel, notice);
    try {
      await onSaved?.({ text: deleteNoticeText({ actorId: actor.id, transcriptChannelId: tc?.id ?? null, dm: wantDm && !dmNote }) + (dmNote ? `\n⚠️ ${dmNote}` : '') });
    } catch (err) {
      logger.debug(`tickets: onSaved failed: ${err?.message}`);
    }

    await sleep(DELETE_DELAY_MS);
    try {
      await channel.delete(reasonFor('delete', actor, t));
    } catch (err) {
      if (err?.code !== 10003) {
        logger.warn(`tickets: deleting channel ${channel.id} failed: ${err?.message}`);
        return { ok: false, code: 'DELETE_FAILED', data: { reason: describeDiscordError(err), transcriptSaved: Boolean(tc) || wantDm } };
      }
    }
    await prisma.ticket.updateMany({
      where: { id: t.id, status: { not: 'DELETED' } },
      data: { status: 'DELETED', closedById: t.closedById ?? actor.id, closedAt: t.closedAt ?? new Date() },
    });
    return { ok: true };
  } catch (err) {
    logger.error('tickets: deleting a ticket failed', err);
    return { ok: false, code: 'UNKNOWN' };
  } finally {
    deleting.delete(t.id);
  }
}

// ── On-demand transcript for a member (shared by /ticket transcript and the Transcript button) ──

/**
 * Staff check, per-ticket throttle and the transcript itself.
 * @returns {Promise<{ ok: true, payload: { embeds: object[], files: object[] } } | { ok: false, code: string, data?: object }>}
 */
export async function transcriptForActor({ guild, ticket, channel, settings, actor }) {
  if (ticket.status === 'DELETED') return { ok: false, code: 'TICKET_DELETED' };
  const can = canPerform('transcript', { isOwner: actor.id === ticket.ownerId, isStaff: isTicketStaff(actor, ticket.panel, settings) });
  if (!can.ok) return can;
  if (!allowTranscript(ticket.id)) return { ok: false, code: 'TRANSCRIPT_THROTTLED' };
  const r = await buildTranscript({ guild, ticket, channel, settings, generatedBy: actor.user });
  if (!r.ok) return { ok: false, code: 'TRANSCRIPT_FAILED', data: { reason: r.reason } };
  return { ok: true, payload: { embeds: [r.embed], files: [r.file] } };
}

// ── Component handlers (§6.7.1, §6.8.6, §6.11) ────────────────────────────────

const replyEphemeral = (interaction, payload) => safeReply(interaction, { ...(typeof payload === 'string' ? { content: payload } : payload), ephemeral: true });
const buttonError = (interaction, payload) => interaction.followUp({ ...payload, ephemeral: true }).catch(() => {});
const notesBlock = (notes) => (notes?.length ? `\n\n${notes.map((n) => `⚠️ ${n}`).join('\n')}` : '');

// Errors before the open flow acknowledges. A dropdown is reset (update) so the same topic can be picked again.
async function ackAndReply(interaction, payload) {
  try {
    if (interaction.isAnySelectMenu?.()) {
      await interaction.update({ components: interaction.message.components });
      await interaction.followUp({ ...payload, ephemeral: true });
    } else {
      await interaction.reply({ ...payload, ephemeral: true });
    }
  } catch (err) {
    if (!isInteractionGone(err)) logger.debug(`tickets: ackAndReply failed: ${err?.message}`);
  }
}

async function replyOpenResult(interaction, r) {
  if (r.ok) {
    return safeReply(interaction, {
      embeds: [successEmbed('Ticket Opened', `Your ticket is ready: <#${r.channel.id}>${notesBlock(r.notes)}`)],
      allowedMentions: { parse: [] },
      ephemeral: true,
    });
  }
  return safeReply(interaction, { ...errorPayload(r), ephemeral: true });
}

/** The Open button, or a dropdown pick: checks, then the optional reason form, otherwise the open flow. */
export async function handleOpenClick(interaction, client, panelId, optionId) {
  if (!interaction.inCachedGuild()) return replyEphemeral(interaction, 'This only works in a server.');
  const { guild } = interaction;
  const guildId = guild.id;
  const isSelect = Boolean(interaction.isAnySelectMenu?.());

  // Everything before the acknowledgement fits the 3 s window (00-overview rule 21): memo first, then a bounded lookup.
  const budget = ackBudgetMs(interaction);
  let panel = peekPanel(guildId, panelId);
  let live = [];
  let lookupFailed = false;
  if (budget > 0) {
    try {
      const [p, l] = await withTimeout(
        Promise.all([panel ?? getPanelForOpen(guildId, panelId), listLiveTickets(guild, guildId, panelId, interaction.user.id)]),
        budget,
        'ticket open lookup',
      );
      panel = p;
      live = l;
    } catch (err) {
      lookupFailed = true;
      if (!(err instanceof TimeoutError)) logger.warn('tickets: open lookup failed', err);
    }
  }
  if (!panel) return ackAndReply(interaction, errorPayload({ code: lookupFailed || budget <= 0 ? 'SLOW' : 'PANEL_GONE' }));

  const option = panel.options.find((o) => o.id === optionId);
  if (!option) return ackAndReply(interaction, errorPayload({ code: 'OPTION_GONE' }));
  if (live.length >= panel.maxOpenPerUser) {
    return ackAndReply(interaction, errorPayload({ code: 'OPEN_LIMIT', data: { ids: live.map((r) => r.channelId), max: panel.maxOpenPerUser } }));
  }

  if (panel.askReason) {
    // showModal is the first response; nothing was acknowledged before it.
    try {
      await interaction.showModal(buildReasonModal(panel.id, option.id, option.label));
    } catch (err) {
      if (!isInteractionGone(err)) logger.warn('tickets: showModal failed', err);
    }
    return undefined;
  }

  try {
    if (isSelect) await interaction.update({ components: interaction.message.components });
    else await interaction.deferReply({ ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('tickets: acknowledging the open click failed', err);
    return undefined;
  }
  const settings = await loadSettings(guildId).catch(() => null);
  const r = await openTicket({ interaction, client, guild, panel, option, reason: null, settings });
  return replyOpenResult(interaction, r);
}

async function handleReasonSubmit(interaction, client, params) {
  if (!interaction.inCachedGuild()) return replyEphemeral(interaction, 'This only works in a server.');
  const { guild } = interaction;

  // This prefix is not wrapped by the module gate (the template form must work while the module is off), so check it here.
  let settings = peekSettings(guild.id);
  if (!settings) {
    const budget = ackBudgetMs(interaction);
    settings = budget > 0 ? await withTimeout(getSettingsSafe(guild.id), budget, 'ticket modal settings').catch(() => null) : null;
  }
  if (settings === null) return replyEphemeral(interaction, 'Server settings are temporarily unavailable. Please try again in a moment.');
  if (!isModuleEnabled(settings, 'tickets')) return replyEphemeral(interaction, errorPayload({ code: 'MODULE_OFF' }));

  const reason = String(interaction.fields.getTextInputValue('ticket-reason') ?? '').trim().slice(0, 1000) || null;
  try {
    if (interaction.isFromMessage()) await interaction.update({ components: interaction.message.components });
    else await interaction.deferReply({ ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('tickets: acknowledging the reason form failed', err);
    return undefined;
  }

  let panel;
  try {
    panel = await getPanelForOpen(guild.id, params[0]);
  } catch (err) {
    logger.warn('tickets: panel lookup after the reason form failed', err);
    return safeReply(interaction, { ...errorPayload({ code: 'SLOW' }), ephemeral: true });
  }
  if (!panel) return safeReply(interaction, { ...errorPayload({ code: 'PANEL_GONE' }), ephemeral: true });
  const option = panel.options.find((o) => o.id === params[1]);
  if (!option) return safeReply(interaction, { ...errorPayload({ code: 'OPTION_GONE' }), ephemeral: true });

  const r = await openTicket({ interaction, client, guild, panel, option, reason, settings });
  return replyOpenResult(interaction, r);
}

const pingRefusal = (refused, channelId) => refused.map((id) => `<@&${id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#${channelId}>.`).join('\n');

// The template form (`ticket-modal:tpl:{panelId}:{panel|intro}[:{optionId}]`). Modal submits bypass the router's level check,
// so the Admin level is checked here as well.
async function handleTemplateSubmit(interaction, client, params) {
  if (!interaction.inCachedGuild()) return replyEphemeral(interaction, 'This only works in a server.');
  const { guild } = interaction;
  let settings = peekSettings(guild.id);
  if (!settings) settings = await withTimeout(getSettingsSafe(guild.id), Math.max(0, ackBudgetMs(interaction)), 'ticket form settings').catch(() => null);
  // ADMIN follows from Discord permissions, so unavailable settings (null) cannot wrongly grant it.
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) return replyEphemeral(interaction, panelErrorPayload('NOT_ADMIN'));
  if (!(await safeDefer(interaction))) return undefined;
  const reply = (payload) => safeReply(interaction, payload);

  const [, panelId, target, optionId] = params;
  if (target !== 'panel' && target !== 'intro') return reply(panelErrorPayload('FORM_EXPIRED'));
  const panel = await prisma.ticketPanel.findFirst({ where: { id: panelId, guildId: guild.id }, include: PANEL_INCLUDE });
  if (!panel) return reply(panelErrorPayload('PANEL_DELETED'));
  let option = null;
  if (optionId) {
    option = panel.options.find((o) => o.id === optionId) ?? null;
    if (!option) return reply(panelErrorPayload('OPTION_NOT_FOUND'));
  }

  const base = target === 'panel'
    ? (panel.template ? normalizeTemplate(panel.template) : defaultPanelTemplate(panel.style))
    : normalizeTemplate(option?.introTemplate ?? panel.introTemplate ?? DEFAULT_INTRO_TEMPLATE);
  const { template, errors } = parseTemplateModal(interaction, base);
  const what = target === 'panel' ? 'panel message' : `ticket intro${option ? ` for **${clampText(option.label, 80)}**` : ''}`;

  const save = async (value) => {
    if (target === 'panel') await prisma.ticketPanel.updateMany({ where: { id: panel.id, guildId: guild.id }, data: { template: value } });
    else if (option) await prisma.ticketPanelOption.updateMany({ where: { id: option.id, panelId: panel.id }, data: { introTemplate: value } });
    else await prisma.ticketPanel.updateMany({ where: { id: panel.id, guildId: guild.id }, data: { introTemplate: value } });
    invalidatePanel(panel.id);
  };

  // An empty form resets the message to its default.
  if (isEmptyTemplate(template)) {
    await save(Prisma.DbNull);
    const refresh = target === 'panel' ? await refreshPanelMessage(guild, panel.id) : null;
    return reply({ ...E(successEmbed('Message Reset', `Restored the default message.${refreshNote(refresh)}`)) });
  }

  if ((template.embed?.description?.length ?? 0) > MODAL_MAX.description) {
    errors.push(`Description is ${template.embed.description.length} characters (max ${MODAL_MAX.description}).`);
  }
  if (errors.length) return reply(panelErrorPayload('INVALID_MESSAGE', { errors }));

  // Configuration time: a {managers} ping must be something the admin could send themselves (00-overview rule 23).
  if (target === 'intro' && templateUsesToken(template, 'managers')) {
    const managers = liveManagerRoleIds(panel, guild);
    const where = guild.channels.cache.get(interaction.channelId) ?? interaction.channel;
    if (managers.length && where) {
      const { refused } = pingableRoleIds(where, interaction.member, managers);
      if (refused.length) return reply(E(errorEmbed('Role Not Pingable', pingRefusal(refused, where.id))));
    }
  }

  const stored = target === 'panel' ? { ...template, buttons: [] } : template;
  await save(toJson(stored));
  const refresh = target === 'panel' ? await refreshPanelMessage(guild, panel.id) : null;

  const sampleVars = {
    number: '0001', reason: 'Example reason', topic: option?.label ?? 'Example topic', panel: panel.name,
    managers: liveManagerRoleIds(panel, guild).map((id) => `<@&${id}>`).join(' '),
  };
  const ctx = { member: interaction.member, user: interaction.user, guild, channel: interaction.channel, vars: target === 'intro' ? sampleVars : {} };
  const unknown = [];
  for (const text of [template.content, template.embed?.title, template.embed?.description, template.embed?.footer?.text]) {
    if (!text) continue;
    for (const token of renderPlaceholdersDetailed(text, ctx).unknownTokens) if (!unknown.includes(token)) unknown.push(token);
  }
  const lines = [
    `Saved the ${what} of **${clampText(panel.name, 80)}**.${refreshNote(refresh)}`,
    ...(unknown.length ? [`Unknown variables (left as typed): ${clampText(unknown.join(', '), 500)}`] : []),
    target === 'intro'
      ? 'Variables: {user} {user.display_name} {number} {reason} {topic} {panel} {managers} {server}'
      : 'Variables: {server} {server.member_count} {channel}',
  ];
  await reply(E(successEmbed('Message Saved', lines.join('\n'))));

  const preview = renderTemplate(template, ctx, {
    defaultColor: settings?.embedColor ?? COLORS.INFO,
    allowedMentions: { parse: [] },
    extraComponents: [],
  });
  if (preview) {
    await interaction.followUp({
      ...preview,
      content: clampText(`**Preview** (nothing is pinged)${preview.content ? `\n${preview.content}` : ''}`, 2000),
      allowedMentions: { parse: [] },
    }).catch((err) => logger.debug(`tickets: preview failed: ${err?.message}`));
  }
  return undefined;
}

/** `ticket-modal` submits: the reason form, or the template form when `params[0] === 'tpl'`. */
export async function handleTicketModal(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  if (params[0] === 'tpl') return handleTemplateSubmit(interaction, client, params);
  return handleReasonSubmit(interaction, client, params);
}

/** The panel dropdown. */
export async function handleTicketSelect(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  const optionId = interaction.values?.[0];
  if (!optionId) return ackAndReply(interaction, errorPayload({ code: 'OPTION_GONE' }));
  return handleOpenClick(interaction, client, params[0], optionId);
}

async function handleLifecycleButton(interaction, client, action, ticketId) {
  if (!interaction.inCachedGuild()) return replyEphemeral(interaction, 'This only works in a server.');
  const isTranscript = action === 'transcript';
  // deferUpdate keeps the clicked message; every error then goes out as an ephemeral follow-up (safeReply would edit the
  // intro or notice message). Only the transcript button uses deferReply.
  try {
    if (isTranscript) await interaction.deferReply({ ephemeral: true });
    else await interaction.deferUpdate();
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('tickets: acknowledging a ticket button failed', err);
    return undefined;
  }
  const fail = (payload) => (isTranscript ? safeReply(interaction, { ...payload, ephemeral: true }) : buttonError(interaction, payload));
  const { guild } = interaction;

  const [settings, ticket] = await Promise.all([loadSettings(guild.id).catch(() => null), getTicketById(guild.id, ticketId)]);
  if (!ticket || ticket.channelId !== interaction.channelId) return fail(errorPayload({ code: 'TICKET_NOT_FOUND' }));

  const ctx = { client, guild, ticket, actor: interaction.member, settings, source: 'button' };
  if (action === 'transcript') {
    const r = await transcriptForActor({ guild, ticket, channel: interaction.channel, settings, actor: interaction.member });
    return r.ok ? safeReply(interaction, { ...r.payload, ephemeral: true }) : fail(errorPayload(r));
  }
  if (action === 'delete') {
    const r = await deleteTicket({ ...ctx, onSaved: async () => {} });
    return r.ok ? undefined : fail(errorPayload(r));
  }

  const run = { claim: claimTicket, close: closeTicket, reopen: reopenTicket }[action];
  const r = await run(ctx);
  if (!r.ok) return fail(errorPayload(r));
  const predicate = action === 'claim' ? (c) => String(c.custom_id ?? '').startsWith('ticket:claim:') : () => true;
  await interaction.editReply({ components: disableButtons(interaction.message.components, predicate) })
    .catch((err) => logger.debug(`tickets: could not update the clicked message: ${err?.message}`));
  if (r.notes.length) await buttonError(interaction, E(warnEmbed('Done, with a note', r.notes.join('\n'))));
  return undefined;
}

/** Every button with the `ticket` prefix (the gate wrapper has already checked the module). */
export async function handleTicketButton(interaction, client) {
  const { params } = parseCustomId(interaction.customId);
  const action = params[0];
  if (action === 'open') return handleOpenClick(interaction, client, params[1], params[2]);
  if (['close', 'claim', 'reopen', 'delete', 'transcript'].includes(action)) return handleLifecycleButton(interaction, client, action, params[1]);
  return replyEphemeral(interaction, 'This button is no longer active.');
}

// ── channelDelete hook (§5.1) ─────────────────────────────────────────────────

/**
 * Cleanup when a channel is deleted: a ticket channel marks its ticket DELETED (no transcript: the history is gone), and a deleted
 * category, transcript channel or panel channel is cleared from the panels. Never throws.
 * @param {{ guildId: string, channel: { id: string, type: number } }} ctx
 */
export async function onChannelDelete(ctx) {
  try {
    const guildId = ctx.guildId;
    const id = ctx.channel.id;
    if (ctx.channel.type === ChannelType.GuildCategory) {
      await prisma.$transaction([
        prisma.ticketPanel.updateMany({ where: { guildId, openCategoryId: id }, data: { openCategoryId: null } }),
        prisma.ticketPanel.updateMany({ where: { guildId, claimedCategoryId: id }, data: { claimedCategoryId: null } }),
        prisma.ticketPanel.updateMany({ where: { guildId, closedCategoryId: id }, data: { closedCategoryId: null } }),
        prisma.ticketPanelOption.updateMany({ where: { categoryId: id, panel: { guildId } }, data: { categoryId: null } }),
      ]);
    } else {
      await prisma.$transaction([
        prisma.ticket.updateMany({ where: { guildId, channelId: id, status: { not: 'DELETED' } }, data: { status: 'DELETED' } }),
        prisma.ticketPanel.updateMany({ where: { guildId, channelId: id }, data: { channelId: null, messageId: null } }),
        prisma.ticketPanel.updateMany({ where: { guildId, transcriptChannelId: id }, data: { transcriptChannelId: null } }),
      ]);
    }
    invalidateGuildPanels(guildId);
  } catch (err) {
    logger.error('tickets: channelDelete cleanup failed', err);
  }
}
