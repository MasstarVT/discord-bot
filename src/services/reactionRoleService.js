// Reaction roles service (pkg-reaction-roles.md).
// Pure helpers (emoji and link parsing, the role-change planner, payload builders), the per-guild panel index used by the
// reaction hooks, the DB access used by /reactionrole, the Discord operations (sync, reactions, role changes), the persistent
// button/select handlers and the hook bodies. Nothing here runs at import: only empty Maps and the module-level helpers.
import {
  ActionRowBuilder,
  ButtonBuilder,
  ButtonStyle,
  EmbedBuilder,
  MessageFlags,
  PermissionFlagsBits,
  StringSelectMenuBuilder,
} from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { getSettingsSafe } from './settingsService.js';
import { COLORS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { createKeyedMutex, createThrottle, mapLimit } from '../utils/async.js';
import { isInteractionGone, safeDefer, safeReply } from '../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { wasDeletedByBot } from '../utils/deletionTracker.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const RR_TYPES = Object.freeze(['NORMAL', 'UNIQUE', 'VERIFY', 'REVERSE']);
export const RR_LAYOUTS = Object.freeze(['BUTTONS', 'SELECT', 'REACTIONS']);
export const MAX_ITEMS = Object.freeze({ BUTTONS: 25, SELECT: 25, REACTIONS: 20 });
export const STYLE_MAP = Object.freeze({ Primary: 1, Secondary: 2, Success: 3, Danger: 4 });
export const TYPE_LABELS = Object.freeze({ NORMAL: 'Normal', UNIQUE: 'Unique', VERIFY: 'Verify', REVERSE: 'Reverse' });
export const LAYOUT_LABELS = Object.freeze({ BUTTONS: 'Buttons', SELECT: 'Dropdown', REACTIONS: 'Emoji reactions' });
export const LIST_PAGE_SIZE = 10;
export const RATE_LIMIT = Object.freeze({ max: 8, windowMs: 10_000 });
export const INDEX_MEMO_TTL_MS = 60_000;
export const INDEX_CACHE_TTL_S = 300;
export const SUPPRESS_TTL_MS = 15_000;
export const indexKey = (guildId) => `guild:${guildId}:rrpanels`;

const INDEX_MEMO_MAX_GUILDS = 10_000;
const SUPPRESS_MAX = 5_000;

export const DEFAULT_TITLE = 'Reaction Roles';
export const DEFAULT_DESCRIPTIONS = Object.freeze({
  BUTTONS: 'Press a button below to manage your roles.',
  SELECT: 'Use the menu below to manage your roles.',
  REACTIONS: 'React below to manage your roles.',
});
export const PLACEHOLDERS = Object.freeze({
  NORMAL: 'Choose the roles you want',
  UNIQUE: 'Choose a role',
  VERIFY: 'Choose a role to get',
  REVERSE: 'Choose a role to remove',
});

// Our own template texts (MEE6's exact wording is not documented), in the MEE6 style.
export const RR_TEMPLATES = Object.freeze({
  verify: Object.freeze({
    type: 'VERIFY', layout: 'BUTTONS', title: '✅ Verification',
    description: Object.freeze({
      BUTTONS: 'Read the rules, then press **Verify** below to unlock the rest of the server.',
      SELECT: 'Read the rules, then pick **Verify** in the menu below to unlock the rest of the server.',
      REACTIONS: 'Read the rules, then react with ✅ below to unlock the rest of the server.',
    }),
    item: Object.freeze({ label: 'Verify', emoji: '✅', style: 3 }),
  }),
  notifications: Object.freeze({
    type: 'NORMAL', layout: 'BUTTONS', title: '🔔 Notification Roles',
    description: Object.freeze({
      BUTTONS: 'Pick the announcements you want to be pinged for. Press a button again to stop.',
      SELECT: 'Pick the announcements you want to be pinged for. Your choice replaces your current notification roles.',
      REACTIONS: 'React to get pinged for an announcement type. Remove your reaction to stop.',
    }),
    item: Object.freeze({ label: null, emoji: '🔔', style: 2 }),
  }),
});

// One standard emoji: a flag, a keycap, or a pictograph with an optional skin tone / tag sequence / ZWJ chain.
// Verified against: ⭐ ❤️ ❤ 👍🏽 👨‍👩‍👧 🇬🇧 1️⃣ #️⃣ 🏴 (tag flag) 🏳️‍🌈 ❤️‍🔥 🫠 ✅ 🔔 ©️ 🎮 (accepted) and a, 1, #, abc, :smile:, 100%, ✅✅ (rejected).
export const UNICODE_EMOJI_RE = /^(?:\p{Regional_Indicator}{2}|[#*0-9]\u{FE0F}?\u{20E3}|(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:\u{FE0F}|\p{Emoji_Modifier})?(?:[\u{E0020}-\u{E007E}]+\u{E007F})?(?:\u{200D}(?:\p{Extended_Pictographic}|\p{Emoji_Presentation})(?:\u{FE0F}|\p{Emoji_Modifier})?)*)$/u;
// Names of 1-32 word characters (the spec says 2-32, but its own test feeds <:x:999999999999999999>); the emoji is still looked up by id.
const CUSTOM_EMOJI_RE = /^<(a)?:(\w{1,32}):(\d{17,20})>$/;

// ── Pure helpers: emoji ───────────────────────────────────────────────────────

let segmenter = null;
function graphemesOf(str) {
  segmenter ??= new Intl.Segmenter('en', { granularity: 'grapheme' });
  return [...segmenter.segment(String(str))].map((s) => s.segment);
}

/** Strips the emoji variation selector so ❤ and ❤️ compare equal. */
export function normalizeEmojiKey(key) {
  return String(key ?? '').replace(/\u{FE0F}/gu, '');
}

/**
 * The reaction key of a stored emoji: the custom emoji id, or the unicode string without U+FE0F.
 * @param {string|null|undefined} stored
 * @returns {string|null}
 */
export function emojiKeyOfStored(stored) {
  if (stored == null || stored === '') return null;
  const s = String(stored);
  const m = CUSTOM_EMOJI_RE.exec(s);
  if (m) return m[3];
  return normalizeEmojiKey(s) || null;
}

/** True when `str` is exactly one grapheme and a standard emoji. */
export function isUnicodeEmoji(str) {
  if (typeof str !== 'string' || str.length === 0) return false;
  try {
    return graphemesOf(str).length === 1 && UNICODE_EMOJI_RE.test(str);
  } catch {
    return false;
  }
}

/**
 * The emoji in the form discord.js accepts on buttons and select options, or null when the stored string is
 * not a valid emoji (legacy rows, R013). Never pass a raw stored string to `setEmoji`.
 * @param {string|null|undefined} stored
 * @returns {{ id: string, name: string, animated: boolean }|{ name: string }|null}
 */
export function componentEmoji(stored) {
  const s = typeof stored === 'string' ? stored.trim() : '';
  if (!s) return null;
  const m = CUSTOM_EMOJI_RE.exec(s);
  if (m) return { id: m[3], name: m[2], animated: Boolean(m[1]) };
  if (isUnicodeEmoji(s)) return { name: s };
  return null;
}

/**
 * Validates and canonicalises an emoji typed by an admin (R013/R061/R066). Never throws.
 * @param {unknown} input
 * @param {{ guild?: any, client?: any }} [ctx]
 * @returns {{ ok: true, emoji: string|null, key: string|null }|{ ok: false, error: string }}
 */
export function parseEmojiInput(input, { guild, client } = {}) {
  try {
    const s = String(input ?? '').trim();
    if (!s) return { ok: true, emoji: null, key: null };

    let id = null;
    const m = CUSTOM_EMOJI_RE.exec(s);
    if (m) {
      id = m[3];
    } else if (/^\d{17,20}$/.test(s)) {
      id = s;
    } else {
      const n = /^:(\w{2,32}):$/.exec(s);
      if (n) {
        const cached = guild?.emojis?.cache;
        const e = cached?.find((x) => x.name === n[1]) ?? cached?.find((x) => x.name?.toLowerCase() === n[1].toLowerCase());
        if (!e) {
          return {
            ok: false,
            error: `I can't find a server emoji called :${n[1]}:. For a standard emoji, pick it from the emoji picker instead of typing its name.`,
          };
        }
        id = e.id;
      }
    }

    if (id) {
      const e = client?.emojis?.cache?.get(id);
      if (!e) return { ok: false, error: "I can't use that emoji. Pick a standard emoji, or a custom emoji from a server I'm in." };
      if (e.available === false) return { ok: false, error: 'That emoji is unavailable (its server may have lost a boost level).' };
      const me = e.guild?.members?.me;
      if (e.roles?.cache?.size > 0 && me && !me.roles.cache.hasAny(...e.roles.cache.keys())) {
        return { ok: false, error: "That emoji is limited to roles I don't have." };
      }
      return { ok: true, emoji: e.toString(), key: e.id };
    }

    if (isUnicodeEmoji(s)) return { ok: true, emoji: s, key: normalizeEmojiKey(s) };

    const parts = graphemesOf(s);
    if (parts.length >= 2 && parts.every((g) => isUnicodeEmoji(g))) return { ok: false, error: 'Use one emoji per role.' };

    if (/^:?[\w+-]{1,40}:?$/.test(s)) {
      return {
        ok: false,
        error: `I can't find a server emoji called :${s.replace(/:/g, '')}:. For a standard emoji, pick it from the emoji picker instead of typing its name.`,
      };
    }
    return { ok: false, error: `"${clampText(s, 40)}" is not an emoji.` };
  } catch (err) {
    logger.warn('parseEmojiInput failed', err);
    return { ok: false, error: 'That emoji could not be read.' };
  }
}

// ── Pure helpers: messages, names, errors ─────────────────────────────────────

const MESSAGE_LINK_RE = /^<?https?:\/\/(?:(?:ptb|canary)\.)?discord(?:app)?\.com\/channels\/(\d{17,20})\/(\d{17,20})\/(\d{17,20})\/?(?:\?\S*)?>?$/;

/**
 * Parses a message link, a `channelId-messageId` pair (Shift + Copy ID) or a bare message id of the current channel.
 * @returns {{ ok: true, channelId: string, messageId: string }|{ ok: false, error: string }}
 */
export function parseMessageReference(input, { guildId, channelId } = {}) {
  const s = String(input ?? '').trim();
  const link = MESSAGE_LINK_RE.exec(s);
  if (link) {
    if (link[1] !== guildId) return { ok: false, error: 'That message is in a different server.' };
    return { ok: true, channelId: link[2], messageId: link[3] };
  }
  const pair = /^(\d{17,20})-(\d{17,20})$/.exec(s);
  if (pair) return { ok: true, channelId: pair[1], messageId: pair[2] };
  if (/^\d{17,20}$/.test(s)) return { ok: true, channelId, messageId: s };
  return { ok: false, error: 'Give a message link (right-click the message → Copy Message Link), or the ID of a message in this channel.' };
}

/** A list name for an attached message: its first text line, its first embed title, or the channel name. Never empty, ≤ 100. */
export function defaultAttachTitle(message, channelName) {
  const lines = String(message?.content ?? '').split(/\r?\n/);
  let title = null;
  for (const line of lines) {
    const t = line.replace(/[*_~`>|#]/g, '').trim();
    if (t) { title = t; break; }
  }
  title ??= message?.embeds?.[0]?.title || null;
  return clampText(title || `Message in #${channelName}`, 100) || 'Attached message';
}

/** Autocomplete name for a panel; always ≤ 100 characters (R063-R065). */
export function panelChoiceName({ title, layout, itemCount }) {
  const suffix = ` · ${LAYOUT_LABELS[layout] ?? layout} · ${itemCount ?? 0} role(s)`;
  return clampText(title, 100 - suffix.length) + suffix;
}

/** Autocomplete name for a panel item; ≤ 100 characters. */
export function itemChoiceName(item, guild) {
  const role = guild?.roles?.cache?.get(item.roleId);
  const prefix = isUnicodeEmoji(item.emoji ?? '') ? `${item.emoji} ` : '';
  return clampText(`${prefix}${item.label} · ${role ? '@' + role.name : 'deleted role'}`, 100);
}

/** Finds an item by item id, by role id or `<@&id>`, then by label (case-insensitive). */
export function resolveItem(group, input) {
  const s = String(input ?? '').trim();
  const items = group?.items ?? [];
  if (!s) return null;
  const byId = items.find((i) => i.id === s);
  if (byId) return byId;
  const role = /^<@&(\d{17,20})>$/.exec(s)?.[1] ?? (/^\d{17,20}$/.test(s) ? s : null);
  if (role) {
    const byRole = items.find((i) => i.roleId === role);
    if (byRole) return byRole;
  }
  const lower = s.toLowerCase();
  return items.find((i) => String(i.label ?? '').trim().toLowerCase() === lower) ?? null;
}

export function discordErrorCode(err) {
  return Number.isInteger(err?.code) ? err.code : null;
}

const ERROR_TEXTS = Object.freeze({
  10003: 'the channel no longer exists',
  10008: 'the message no longer exists',
  10011: 'that role no longer exists',
  10014: "I can't use that emoji",
  30010: 'that message already has 20 different reactions',
  50001: "I can't see that channel",
  50005: "I can't edit a message someone else sent",
  50013: "I'm missing permissions there",
  50035: 'Discord rejected the panel content',
  50083: 'the thread is archived',
});

/** Lowercase, no final period, so callers can embed it in a sentence. Takes an Error or a `{ code }` result. */
export function describeDiscordError(errOrResult) {
  const code = discordErrorCode(errOrResult);
  if (code !== null && Object.hasOwn(ERROR_TEXTS, code)) return ERROR_TEXTS[code];
  return `Discord error ${code ?? 'unknown'}`;
}

// ── Payload builders ──────────────────────────────────────────────────────────

const FOOTERS = Object.freeze({
  NORMAL: Object.freeze({
    BUTTONS: 'Press a button again to remove its role',
    SELECT: 'Your selection replaces your roles from this panel',
    REACTIONS: 'Remove your reaction to remove the role',
  }),
  UNIQUE: 'You can have one role from this panel',
  VERIFY: "Roles from this panel can't be removed here",
  REVERSE: Object.freeze({
    BUTTONS: 'Pressing a button removes that role',
    SELECT: 'Picking a role removes it',
    REACTIONS: 'Reacting removes the role; remove your reaction to get it back',
  }),
});

/** The footer line of a panel for its mode, layout and role cap. */
export function panelFooter(group) {
  const f = FOOTERS[group.type] ?? FOOTERS.NORMAL;
  let text = typeof f === 'string' ? f : (f[group.layout] ?? f.BUTTONS);
  if (group.type === 'NORMAL' && group.maxRoles) text += ` · Max ${group.maxRoles} role(s)`;
  return text;
}

const sortedItems = (group) => [...(group.items ?? [])].sort((a, b) => (a.position ?? 0) - (b.position ?? 0));

/**
 * The select menu row for a dropdown panel, or null without items.
 * @param {{ id: string, type: string, maxRoles?: number|null, placeholder?: string|null, items: any[] }} group
 * @param {{ selectedRoleIds?: string[]|null, placeholder?: string|null }} [opts]
 * @returns {ActionRowBuilder|null}
 */
export function buildSelectRow(group, { selectedRoleIds = null, placeholder = null } = {}) {
  const items = sortedItems(group).slice(0, MAX_ITEMS.SELECT);
  if (items.length === 0) return null;
  const max = group.type === 'UNIQUE'
    ? 1
    : Math.min(items.length, group.type === 'NORMAL' && group.maxRoles ? group.maxRoles : 25);
  // Discord rejects more defaults than max_values; legacy data could hold more roles than the cap.
  const selected = new Set((selectedRoleIds ?? []).filter((id) => items.some((i) => i.roleId === id)).slice(0, max));
  const menu = new StringSelectMenuBuilder()
    .setCustomId(buildCustomId('rr-select', group.id))
    .setPlaceholder(clampText(placeholder ?? group.placeholder ?? PLACEHOLDERS[group.type] ?? PLACEHOLDERS.NORMAL, 150))
    .setMinValues(0)
    .setMaxValues(max);
  menu.addOptions(items.map((item) => {
    const option = {
      label: clampText(item.label, 100) || '—',
      value: item.roleId,
      default: selected.has(item.roleId),
    };
    const emoji = componentEmoji(item.emoji);
    if (emoji) option.emoji = emoji;
    if (item.description) option.description = clampText(item.description, 100);
    return option;
  }));
  return new ActionRowBuilder().addComponents(menu);
}

/**
 * The message payload of a panel. Pure; never throws for stored data (bad emojis are dropped, text is clamped).
 * @param {object} group needs id, title, description, type, layout, maxRoles, plainText, messageContent, placeholder, items[]
 * @param {{ color?: number|null }} [opts]
 * @returns {{ content: string|null, embeds: EmbedBuilder[], components: ActionRowBuilder[], allowedMentions: { parse: [] } }}
 */
export function buildPanelPayload(group, { color } = {}) {
  const items = sortedItems(group);
  let listing = null;
  if (items.length === 0) listing = '*No roles have been added to this panel yet.*';
  else if (group.layout === 'REACTIONS') {
    listing = items.map((i) => `${componentEmoji(i.emoji) ? i.emoji : '▫️'} — **${i.label}**`).join('\n');
  }
  const body = [group.description ?? DEFAULT_DESCRIPTIONS[group.layout] ?? DEFAULT_DESCRIPTIONS.BUTTONS, listing].filter(Boolean).join('\n\n');
  const footer = panelFooter(group);
  const title = clampText(group.title, 256) || DEFAULT_TITLE;

  let content;
  let embeds;
  if (!group.plainText) {
    const embed = new EmbedBuilder()
      .setColor(Number.isInteger(color) && color >= 0 && color <= 0xffffff ? color : COLORS.INFO)
      .setTitle(title)
      .setDescription(clampText(body, 4096))
      .setFooter({ text: clampText(footer, 2048) });
    embeds = [embed];
    content = group.messageContent ? clampText(group.messageContent, 2000) : null;
  } else {
    embeds = [];
    content = group.messageContent
      ? clampText([group.messageContent, listing, `-# ${footer}`].filter(Boolean).join('\n\n'), 2000)
      : clampText([`**${title}**`, body, `-# ${footer}`].filter(Boolean).join('\n'), 2000);
  }

  let components = [];
  if (items.length > 0) {
    if (group.layout === 'BUTTONS') {
      const shown = items.slice(0, MAX_ITEMS.BUTTONS);
      for (let i = 0; i < shown.length; i += 5) {
        components.push(new ActionRowBuilder().addComponents(shown.slice(i, i + 5).map((item) => {
          const button = new ButtonBuilder()
            .setCustomId(item.customId)
            .setLabel(clampText(item.label, 80) || '—')
            .setStyle([1, 2, 3, 4].includes(item.style) ? item.style : ButtonStyle.Primary);
          const emoji = componentEmoji(item.emoji);
          if (emoji) button.setEmoji(emoji);
          return button;
        })));
      }
    } else if (group.layout === 'SELECT') {
      const row = buildSelectRow(group);
      components = row ? [row] : [];
    }
  }

  return { content, embeds, components, allowedMentions: { parse: [] } };
}

/** A new item row for a panel: its customId is always `rr:{groupId}:{roleId}`. */
export function draftItem(groupId, { roleId, label, emoji = null, style = 1, description = null }, position) {
  return { customId: buildCustomId('rr', groupId, roleId), roleId, label, emoji, style, description, position };
}

/** Maps panel items to `{ roleId, emoji, key }`. */
export function indexItems(group) {
  return (group?.items ?? []).map((i) => ({ roleId: i.roleId, emoji: i.emoji ?? null, key: emojiKeyOfStored(i.emoji) }));
}

// ── Role-change planner (pure) ────────────────────────────────────────────────

/**
 * What to change for one member, for one click, selection or reaction (spec section 6.5).
 * Held roles must come from a member read inside the member mutex.
 * @param {{ type: string, source: 'button'|'select'|'react'|'unreact', panelRoleIds: string[], heldRoleIds: string[],
 *   targetRoleIds: string[], maxRoles?: number|null }} input
 * @returns {{ toAdd: string[], toRemove: string[], skipped: string[], reason: null|'max'|'already'|'not-held'|'no-change' }}
 */
export function planRoleChange({ type, source, panelRoleIds, heldRoleIds, targetRoleIds, maxRoles = null }) {
  const P = [...new Set(panelRoleIds ?? [])];
  const held = new Set(heldRoleIds ?? []);
  const want = new Set(targetRoleIds ?? []);
  const H = P.filter((id) => held.has(id));
  const T = P.filter((id) => want.has(id)); // ids that are not on the panel are dropped here (R163)
  const max = type === 'NORMAL' && maxRoles ? maxRoles : null;
  const plan = { toAdd: [], toRemove: [], skipped: [], reason: null };
  const done = (reason = null) => {
    plan.reason = reason;
    if (plan.reason === null && plan.toAdd.length === 0 && plan.toRemove.length === 0) plan.reason = 'no-change';
    return plan;
  };

  if (source !== 'select' && T.length === 0) return done('no-change');
  const r = T[0];

  if (source === 'select') {
    if (type === 'NORMAL') {
      const keep = H.filter((id) => T.includes(id));
      const adds = T.filter((id) => !held.has(id));
      plan.toRemove = H.filter((id) => !T.includes(id));
      if (max && keep.length + adds.length > max) {
        const room = Math.max(0, max - keep.length);
        plan.toAdd = adds.slice(0, room);
        plan.skipped = adds.slice(room);
      } else {
        plan.toAdd = adds;
      }
      // Nothing changes but part of the choice was refused by the cap: say so instead of "no changes".
      if (plan.toAdd.length === 0 && plan.toRemove.length === 0 && plan.skipped.length > 0) return done('max');
      return done();
    }
    if (type === 'UNIQUE') {
      if (T.length === 0) { plan.toRemove = H; return done(); }
      const t = T[0];
      if (!held.has(t)) plan.toAdd = [t];
      plan.toRemove = H.filter((id) => id !== t);
      return done();
    }
    if (type === 'VERIFY') {
      if (T.length === 0) return done('no-change');
      plan.toAdd = T.filter((id) => !held.has(id));
      return done(plan.toAdd.length === 0 ? 'already' : null);
    }
    if (type === 'REVERSE') {
      if (T.length === 0) return done('no-change');
      plan.toRemove = T.filter((id) => held.has(id));
      return done(plan.toRemove.length === 0 ? 'not-held' : null);
    }
    return done('no-change');
  }

  const has = held.has(r);
  switch (type) {
    case 'NORMAL':
      if (source === 'button') {
        if (has) { plan.toRemove = [r]; return done(); }
        if (max && H.length >= max) return done('max');
        plan.toAdd = [r];
        return done();
      }
      if (source === 'react') {
        if (has) return done('no-change');
        if (max && H.length >= max) return done('max');
        plan.toAdd = [r];
        return done();
      }
      if (has) plan.toRemove = [r]; // unreact
      return done();
    case 'UNIQUE':
      if (source === 'button') {
        if (has) { plan.toRemove = H; return done(); }
        plan.toAdd = [r];
        plan.toRemove = H.filter((id) => id !== r);
        return done();
      }
      if (source === 'react') {
        if (!has) plan.toAdd = [r];
        plan.toRemove = H.filter((id) => id !== r);
        return done();
      }
      if (has) plan.toRemove = [r]; // unreact
      return done();
    case 'VERIFY':
      if (source === 'button') {
        if (has) return done('already');
        plan.toAdd = [r];
        return done();
      }
      if (source === 'react' && !has) plan.toAdd = [r];
      return done(); // unreact never removes
    case 'REVERSE':
      if (source === 'button') {
        if (has) { plan.toRemove = [r]; return done(); }
        return done('not-held');
      }
      if (source === 'react') {
        if (has) plan.toRemove = [r];
        return done();
      }
      if (!has) plan.toAdd = [r]; // unreact: the role comes back
      return done();
    default:
      return done('no-change');
  }
}

// ── Panel index (cache for the reaction hooks) ────────────────────────────────

/** @typedef {{ id: string, channelId: string, messageId: string, title: string, type: string, layout: string, maxRoles: number|null,
 *   attached: boolean, items: Array<{ roleId: string, emoji: string|null, key: string|null }> }} PanelLite */

/** @type {Map<string, { at: number, byMessageId: Map<string, PanelLite> }>} */
const memo = new Map();
/** @type {Map<string, { gen: number, promise: Promise<Map<string, PanelLite>|null> }>} */
const inflight = new Map();
/** Bumped on every invalidation so a load that started before it can never memoise stale rows. */
const generations = new Map();
const indexErrorThrottle = createThrottle(60_000);

const genOf = (guildId) => generations.get(guildId) ?? 0;

function bumpGeneration(guildId) {
  const next = genOf(guildId) + 1;
  generations.delete(guildId); // refresh the insertion order
  generations.set(guildId, next);
  while (generations.size > INDEX_MEMO_MAX_GUILDS) generations.delete(generations.keys().next().value);
}

function setMemo(guildId, panels) {
  const byMessageId = new Map(panels.map((p) => [p.messageId, p]));
  memo.delete(guildId);
  memo.set(guildId, { at: Date.now(), byMessageId });
  while (memo.size > INDEX_MEMO_MAX_GUILDS) memo.delete(memo.keys().next().value);
  return byMessageId;
}

function toPanelLite(row) {
  return {
    id: row.id,
    channelId: row.channelId,
    messageId: row.messageId,
    title: row.title,
    type: row.type,
    layout: row.layout,
    maxRoles: row.maxRoles ?? null,
    attached: Boolean(row.attached),
    items: (row.items ?? []).map((i) => ({ roleId: i.roleId, emoji: i.emoji ?? null, key: emojiKeyOfStored(i.emoji) })),
  };
}

function isPanelLite(p) {
  return p && typeof p.id === 'string' && typeof p.messageId === 'string' && Array.isArray(p.items);
}

/** Reads only the in-process memo. `fresh` is false once it is older than INDEX_MEMO_TTL_MS. */
export function peekPanelIndex(guildId) {
  const entry = memo.get(guildId);
  if (!entry) return null;
  return { fresh: Date.now() - entry.at < INDEX_MEMO_TTL_MS, byMessageId: entry.byMessageId };
}

/** Test-only: primes the memo with PanelLite rows. */
export function __setPanelIndex(guildId, panels) {
  return setMemo(guildId, panels);
}

/** Test-only: forgets every memo, in-flight load and generation. */
export function __resetPanelIndex() {
  memo.clear();
  inflight.clear();
  generations.clear();
}

async function loadIndex(guildId, gen) {
  // Redis miss or outage -> fail OPEN to Postgres (a cache miss is never a deletion).
  const hit = await cache.get(indexKey(guildId));
  if (Array.isArray(hit) && hit.every(isPanelLite) && genOf(guildId) === gen) {
    return setMemo(guildId, hit);
  }
  let rows;
  try {
    rows = await prisma.reactionRoleGroup.findMany({
      where: { guildId },
      select: {
        id: true, channelId: true, messageId: true, title: true, type: true, layout: true, maxRoles: true, attached: true,
        items: { select: { roleId: true, emoji: true }, orderBy: { position: 'asc' } },
      },
    });
  } catch (err) {
    // Postgres down -> fail CLOSED: callers skip the event (no role change on an unknown panel).
    if (indexErrorThrottle(guildId)) logger.error(`reaction-roles: could not load the panels of guild ${guildId}`, err);
    return null;
  }
  const panels = rows.map(toPanelLite);
  if (genOf(guildId) !== gen) {
    // Invalidated while loading: use the rows for this call only.
    return new Map(panels.map((p) => [p.messageId, p]));
  }
  const byMessageId = setMemo(guildId, panels);
  await cache.set(indexKey(guildId), panels, INDEX_CACHE_TTL_S);
  return byMessageId;
}

/**
 * Panels of a guild by message id, or null when they cannot be read (callers then skip the event).
 * Concurrent loads for one guild share a promise.
 * @param {string} guildId
 * @returns {Promise<Map<string, PanelLite>|null>}
 */
export async function getPanelIndex(guildId) {
  const entry = memo.get(guildId);
  if (entry && Date.now() - entry.at < INDEX_MEMO_TTL_MS) return entry.byMessageId;
  const gen = genOf(guildId);
  const running = inflight.get(guildId);
  if (running && running.gen === gen) return running.promise;
  const promise = loadIndex(guildId, gen).catch((err) => {
    logger.error(`reaction-roles: panel index failed for guild ${guildId}`, err);
    return null;
  });
  const record = { gen, promise };
  inflight.set(guildId, record);
  const clear = () => { if (inflight.get(guildId) === record) inflight.delete(guildId); };
  promise.then(clear, clear);
  return promise;
}

/** Drops the memo and the Redis copy. Call after every mutation, before Discord calls that can fire our own hooks. */
export async function invalidatePanelIndex(guildId) {
  bumpGeneration(guildId);
  memo.delete(guildId);
  inflight.delete(guildId);
  await cache.del(indexKey(guildId));
}

// ── Other in-process state ────────────────────────────────────────────────────

const memberMutex = createKeyedMutex();
const warnThrottle = createThrottle(10 * 60_000);
const restoreThrottle = createThrottle(60_000);
/** @type {Map<string, number>} key `${messageId}:${userId}:${key}` -> expiresAt */
const suppressed = new Map();
/** @type {Set<Promise<unknown>>} fire-and-forget work, so tests can wait for it */
const background = new Set();

function runInBackground(label, fn) {
  const p = Promise.resolve().then(fn).catch((err) => logger.error(`reaction-roles ${label} failed`, err));
  background.add(p);
  p.then(() => background.delete(p));
  return p;
}

/** Test-only: resolves when every background task has finished. */
export async function __idle() {
  while (background.size) await Promise.all([...background]);
}

/** Marks a reaction the BOT is about to remove from a member, so the resulting remove event runs no role logic. */
export function suppressReactionRemoval(messageId, userId, key) {
  const k = `${messageId}:${userId}:${key}`;
  suppressed.delete(k);
  suppressed.set(k, Date.now() + SUPPRESS_TTL_MS);
  while (suppressed.size > SUPPRESS_MAX) suppressed.delete(suppressed.keys().next().value);
}

/** True once when the bot removed this reaction itself (and the mark has not expired); consuming deletes it. */
export function consumeReactionSuppression(messageId, userId, key) {
  const k = `${messageId}:${userId}:${key}`;
  const expires = suppressed.get(k);
  if (expires === undefined) return false;
  suppressed.delete(k);
  return expires > Date.now();
}

// ── DB access (the command calls only these) ──────────────────────────────────

const withItems = { items: { orderBy: { position: 'asc' } } };

export function getPanel(guildId, panelId) {
  return prisma.reactionRoleGroup.findFirst({ where: { id: String(panelId), guildId }, include: withItems });
}

export function countPanels(guildId) {
  return prisma.reactionRoleGroup.count({ where: { guildId } });
}

export function findPanelByMessage(guildId, messageId) {
  return prisma.reactionRoleGroup.findFirst({ where: { guildId, messageId }, select: { id: true, title: true } });
}

export async function searchPanels(guildId, q) {
  const rows = await prisma.reactionRoleGroup.findMany({
    where: {
      guildId,
      ...(q ? { OR: [{ title: { contains: q, mode: 'insensitive' } }, { messageId: { contains: q } }, { id: q }] } : {}),
    },
    select: { id: true, title: true, type: true, layout: true, attached: true, _count: { select: { items: true } } },
    orderBy: { createdAt: 'desc' },
    take: 25,
  });
  return rows.map(({ _count, ...rest }) => ({ ...rest, itemCount: _count.items }));
}

export function searchItems(guildId, panelId, q, roleIds = []) {
  return prisma.reactionRoleItem.findMany({
    where: {
      groupId: panelId,
      group: { guildId },
      ...(q
        ? {
            OR: [
              { label: { contains: q, mode: 'insensitive' } },
              { roleId: { contains: q } },
              ...(roleIds.length ? [{ roleId: { in: roleIds } }] : []),
            ],
          }
        : {}),
    },
    orderBy: { position: 'asc' },
    take: 25,
    select: { id: true, roleId: true, label: true, emoji: true },
  });
}

export async function listPanels(guildId) {
  const rows = await prisma.reactionRoleGroup.findMany({
    where: { guildId },
    select: {
      id: true, title: true, type: true, layout: true, maxRoles: true, attached: true, plainText: true,
      channelId: true, messageId: true, messageMissingAt: true, _count: { select: { items: true } },
    },
    orderBy: { createdAt: 'desc' },
  });
  return rows.map(({ _count, ...rest }) => ({ ...rest, itemCount: _count.items }));
}

/**
 * Creates a panel with its items in one transaction.
 * @param {object} draft group fields (explicit `id`) plus `items` built with draftItem()
 */
export function createPanel(draft) {
  const {
    id, guildId, channelId, messageId, title, description = null, type, layout, maxRoles = null, plainText = false,
    messageContent = null, attached = false, placeholder = null, items = [],
  } = draft;
  return prisma.$transaction(async (tx) => {
    await tx.reactionRoleGroup.create({
      data: { id, guildId, channelId, messageId, title, description, type, layout, maxRoles, plainText, messageContent, attached, placeholder },
    });
    for (const it of items) {
      await tx.reactionRoleItem.create({
        data: {
          groupId: id,
          customId: it.customId,
          roleId: it.roleId,
          label: it.label,
          emoji: it.emoji ?? null,
          style: it.style ?? 1,
          position: it.position ?? 0,
          description: it.description ?? null,
        },
      });
    }
  });
}

/** Adds an item at the end. A duplicate role rejects with Prisma's P2002, unchanged. */
export function addItem(group, { roleId, label, emoji = null, style = 1, description = null }) {
  const position = group.items?.length ? Math.max(...group.items.map((i) => i.position)) + 1 : 0;
  return prisma.reactionRoleItem.create({ data: { groupId: group.id, ...draftItem(group.id, { roleId, label, emoji, style, description }, position) } });
}

/**
 * Adds items and sets every item's position from `order` (role ids, first to last) in one transaction. A role missing
 * from `order` keeps its relative place after the listed ones.
 */
export function insertItems(group, items, order) {
  const all = orderItems([...group.items, ...items], order);
  return prisma.$transaction(async (tx) => {
    for (const [position, it] of all.entries()) {
      if (it.id) {
        if (it.position !== position) await tx.reactionRoleItem.update({ where: { id: it.id }, data: { position } });
      } else {
        await tx.reactionRoleItem.create({ data: { groupId: group.id, ...draftItem(group.id, it, position) } });
      }
    }
  });
}

/** `items` sorted by their role's place in `order`; roles not in it go last, in their given order. */
export function orderItems(items, order) {
  const rank = (roleId) => (order.includes(roleId) ? order.indexOf(roleId) : order.length);
  return items.map((it, i) => ({ it, i })).sort((a, b) => rank(a.it.roleId) - rank(b.it.roleId) || a.i - b.i).map(({ it }) => it);
}

/** This guild's Unique dropdown and button panels that the bot posted, with their items (template panels among them). */
export function listUniquePanels(guildId) {
  return prisma.reactionRoleGroup.findMany({
    where: { guildId, type: 'UNIQUE', attached: false, layout: { in: ['SELECT', 'BUTTONS'] } },
    include: withItems,
  });
}

/** Deletes one item and renumbers the positions 0..n-1 in one transaction. */
export function removeItem(group, itemId) {
  return prisma.$transaction(async (tx) => {
    await tx.reactionRoleItem.delete({ where: { id: itemId } });
    const rest = await tx.reactionRoleItem.findMany({ where: { groupId: group.id }, orderBy: { position: 'asc' } });
    for (let i = 0; i < rest.length; i++) {
      if (rest[i].position !== i) await tx.reactionRoleItem.update({ where: { id: rest[i].id }, data: { position: i } });
    }
  });
}

/** Updates a panel of this guild and returns it with its items, or null when it was deleted meanwhile. */
export async function updatePanel(guildId, panelId, data) {
  await prisma.reactionRoleGroup.updateMany({ where: { id: panelId, guildId }, data });
  return getPanel(guildId, panelId);
}

/** Deletes a panel of this guild (items cascade). */
export function deletePanel(guildId, panelId) {
  return prisma.reactionRoleGroup.deleteMany({ where: { id: panelId, guildId } });
}

/** Points a panel at a new message and clears the "message deleted" marker. */
export function movePanel(guildId, panelId, { channelId, messageId, attached }) {
  return prisma.reactionRoleGroup.updateMany({
    where: { id: panelId, guildId },
    data: { channelId, messageId, attached, messageMissingAt: null },
  });
}

/** Flags a panel whose message or channel is gone (only while the guild is available: rule 20). Never throws. */
export async function markPanelMissing(guild, group) {
  try {
    if (!guild || guild.available === false || !group?.id) return;
    await prisma.reactionRoleGroup.updateMany({
      where: { id: group.id, guildId: guild.id, messageMissingAt: null },
      data: { messageMissingAt: new Date() },
    });
  } catch (err) {
    logger.warn(`reaction-roles: could not flag panel ${group?.id} as missing`, err);
  }
}

// ── Discord operations ────────────────────────────────────────────────────────

/**
 * Fetches a panel's channel and message with REST fallbacks.
 * @returns {Promise<{ status: 'ok', channel: any, message: any }|{ status: 'missing'|'failed', code: number|null, reason: string }>}
 */
export async function fetchPanelMessage(guild, group) {
  let channel = guild.channels?.cache?.get(group.channelId) ?? null;
  if (!channel) {
    try {
      channel = await guild.channels.fetch(group.channelId);
    } catch (err) {
      const code = discordErrorCode(err);
      if (code !== 10003) return { status: 'failed', code, reason: describeDiscordError(err) };
    }
    if (!channel) {
      // REST said Unknown Channel (10003): a cache miss alone never gets here (rule 20).
      await markPanelMissing(guild, group);
      return { status: 'missing', code: 10003, reason: describeDiscordError({ code: 10003 }) };
    }
  }
  if (!channel.isTextBased?.()) return { status: 'failed', code: null, reason: 'that is not a text channel' };
  try {
    const message = await channel.messages.fetch(group.messageId);
    return { status: 'ok', channel, message };
  } catch (err) {
    const code = discordErrorCode(err);
    if (code === 10008) {
      await markPanelMissing(guild, group);
      return { status: 'missing', code, reason: describeDiscordError(err) };
    }
    return { status: 'failed', code, reason: describeDiscordError(err) };
  }
}

/**
 * Re-renders a panel's message. Attached messages are never edited.
 * @param {any} guild
 * @param {string|object} groupOrId a panel id, or a panel row with its items
 * @param {{ settings?: { embedColor?: number|null }|null }} [opts]
 * @returns {Promise<{ status: 'ok'|'skipped'|'missing'|'failed', code?: number|null, reason?: string }>}
 */
export async function syncPanel(guild, groupOrId, { settings = null } = {}) {
  try {
    const group = typeof groupOrId === 'string' ? await getPanel(guild.id, groupOrId) : groupOrId;
    if (!group) return { status: 'skipped' };
    if (group.attached) return { status: 'skipped' };
    const fm = await fetchPanelMessage(guild, group);
    if (fm.status !== 'ok') return { status: fm.status, code: fm.code, reason: fm.reason };
    const payload = buildPanelPayload(group, { color: settings?.embedColor });
    try {
      await fm.message.edit(payload);
    } catch (err) {
      const code = discordErrorCode(err);
      if (code === 10008) {
        await markPanelMissing(guild, group);
        return { status: 'missing', code, reason: describeDiscordError(err) };
      }
      logger.warn(`reaction-roles: could not edit panel ${group.id} in ${guild.id}: ${err?.message ?? err}`);
      return { status: 'failed', code, reason: describeDiscordError(err) };
    }
    if (group.messageMissingAt) {
      await prisma.reactionRoleGroup.updateMany({ where: { id: group.id, guildId: guild.id }, data: { messageMissingAt: null } }).catch(() => {});
    }
    return { status: 'ok' };
  } catch (err) {
    logger.warn(`reaction-roles: syncPanel failed in ${guild?.id}`, err);
    return { status: 'failed', code: discordErrorCode(err), reason: describeDiscordError(err) };
  }
}

/** Adds the bot's reaction. @returns {Promise<{ ok: boolean, code?: number|null, reason?: string }>} */
export async function addPanelReaction(message, stored) {
  try {
    await message.react(stored);
    return { ok: true };
  } catch (err) {
    return { ok: false, code: discordErrorCode(err), reason: describeDiscordError(err) };
  }
}

const reactionKey = (r) => normalizeEmojiKey(r.emoji?.id ?? r.emoji?.name);

/**
 * Makes sure the bot has reacted with every item emoji, in position order (sequential: discord.js queues 429s).
 * @returns {Promise<{ added: number, failed: Array<{ emoji: string, code: number|null, reason: string }> }>}
 */
export async function ensurePanelReactions(message, items) {
  const out = { added: 0, failed: [] };
  for (const item of items ?? []) {
    if (!componentEmoji(item.emoji)) continue;
    const key = item.key ?? emojiKeyOfStored(item.emoji);
    const existing = message.reactions?.cache?.find((r) => reactionKey(r) === key);
    if (existing?.me) continue;
    const r = await addPanelReaction(message, item.emoji);
    if (r.ok) out.added++;
    else out.failed.push({ emoji: item.emoji, code: r.code ?? null, reason: r.reason ?? 'unknown error' });
  }
  return out;
}

function hasManageMessages(guild, channelId, fallbackChannel = null) {
  try {
    const channel = guild?.channels?.cache?.get(channelId) ?? fallbackChannel;
    const me = guild?.members?.me;
    return Boolean(channel && me && channel.permissionsFor?.(me)?.has(PermissionFlagsBits.ManageMessages));
  } catch {
    return false;
  }
}

/**
 * Removes an item's reaction from a panel message: everyone's with Manage Messages, otherwise only the bot's own.
 * @returns {Promise<{ ok: boolean, all: boolean }>}
 */
export async function removePanelReaction(message, stored, { client } = {}) {
  try {
    const key = emojiKeyOfStored(stored);
    const reaction = message.reactions?.cache?.find((r) => reactionKey(r) === key);
    if (!reaction) return { ok: true, all: false };
    const guild = message.guild ?? client?.guilds?.cache?.get(message.guildId);
    if (hasManageMessages(guild, message.channelId, message.channel)) {
      await reaction.remove();
      return { ok: true, all: true };
    }
    await reaction.users.remove(client.user.id);
    return { ok: true, all: false };
  } catch (err) {
    logger.debug(`reaction-roles: could not remove a reaction: ${err?.message ?? err}`);
    return { ok: false, all: false };
  }
}

/** Removes only the bot's own reactions for these items (attached panels, never anyone else's). */
export async function removeOwnReactions(message, items, client) {
  for (const item of items ?? []) {
    try {
      const key = item.key ?? emojiKeyOfStored(item.emoji);
      const reaction = message.reactions?.cache?.find((r) => reactionKey(r) === key);
      if (reaction?.me) await reaction.users.remove(client.user.id);
    } catch (err) {
      logger.debug(`reaction-roles: could not remove an own reaction: ${err?.message ?? err}`);
    }
  }
}

/**
 * Which of these roles the bot cannot use right now. `missing` comes from the role CACHE, so it is only a
 * suspicion: confirm with a REST fetch before treating a role as deleted (rule 20).
 * @returns {{ missing: string[], blocked: string[], noManageRoles: boolean }}
 */
export function checkRolesUsable(guild, roleIds) {
  const me = guild.members?.me;
  const noManageRoles = !me?.permissions?.has(PermissionFlagsBits.ManageRoles);
  const missing = [];
  const blocked = [];
  for (const id of new Set(roleIds)) {
    const role = guild.roles.cache.get(id);
    if (!role) missing.push(id);
    else if (noManageRoles || !role.editable) blocked.push(id);
  }
  return { missing, blocked, noManageRoles };
}

/** 'gone' only for a REST 10011 (fetch returns null) while the guild is available; otherwise 'exists' or 'unknown'. */
async function roleVerdict(guild, roleId) {
  if (guild.available === false) return 'unknown';
  try {
    const role = await guild.roles.fetch(roleId, { force: true });
    return role ? 'exists' : 'gone';
  } catch {
    return 'unknown';
  }
}

/** checkRolesUsable with the cache-miss suspicions confirmed over REST. */
async function verifyRoles(guild, roleIds) {
  let u = checkRolesUsable(guild, roleIds);
  const gone = [];
  if (u.missing.length) {
    for (const id of u.missing) if ((await roleVerdict(guild, id)) === 'gone') gone.push(id);
    u = checkRolesUsable(guild, roleIds); // roles that exist are cached by the fetch
  }
  return { gone, unverified: u.missing.filter((id) => !gone.includes(id)), blocked: u.blocked, noManageRoles: u.noManageRoles };
}

/**
 * Applies a plan one role id at a time: adds first (a failed add stops everything, so a failed swap keeps the old role),
 * then removes. Never uses the array form, which would PATCH the whole role list (R161).
 * @returns {Promise<{ added: string[], removed: string[], failed: { roleId: string, op: 'add'|'remove', code: number|null }|null }>}
 */
export async function applyRoleChange(member, plan, { reason } = {}) {
  const out = { added: [], removed: [], failed: null };
  for (const roleId of plan.toAdd) {
    if (member.roles.cache.has(roleId)) continue;
    try {
      await member.roles.add(roleId, reason);
      out.added.push(roleId);
    } catch (err) {
      out.failed = { roleId, op: 'add', code: discordErrorCode(err) };
      return out;
    }
  }
  for (const roleId of plan.toRemove) {
    if (!member.roles.cache.has(roleId)) continue;
    try {
      await member.roles.remove(roleId, reason);
      out.removed.push(roleId);
    } catch (err) {
      out.failed = { roleId, op: 'remove', code: discordErrorCode(err) };
      return out;
    }
  }
  return out;
}

/**
 * Deletes the items of a deleted role (after a roleDelete event or a confirmed 10011) and re-renders the panels. Never throws.
 * @returns {Promise<number>} how many items were removed
 */
export async function pruneItemsForRole(guild, roleId, { client } = {}) {
  try {
    if (!guild || guild.available === false) return 0;
    const items = await prisma.reactionRoleItem.findMany({
      where: { roleId, group: { guildId: guild.id } },
      select: { id: true, groupId: true, emoji: true },
    });
    if (items.length === 0) return 0;
    const groupIds = [...new Set(items.map((i) => i.groupId))];
    await prisma.$transaction(async (tx) => {
      await tx.reactionRoleItem.deleteMany({ where: { id: { in: items.map((i) => i.id) } } });
      for (const groupId of groupIds) {
        const rest = await tx.reactionRoleItem.findMany({ where: { groupId }, orderBy: { position: 'asc' } });
        for (let i = 0; i < rest.length; i++) {
          if (rest[i].position !== i) await tx.reactionRoleItem.update({ where: { id: rest[i].id }, data: { position: i } });
        }
      }
    });
    await invalidatePanelIndex(guild.id);

    runInBackground('prune re-sync', async () => {
      const settings = await getSettingsSafe(guild.id);
      await mapLimit(groupIds, 2, async (groupId) => {
        const group = await getPanel(guild.id, groupId);
        if (!group) return;
        await syncPanel(guild, group, { settings });
        if (group.layout !== 'REACTIONS') return;
        const fm = await fetchPanelMessage(guild, group);
        if (fm.status !== 'ok') return;
        for (const item of items.filter((i) => i.groupId === groupId && i.emoji)) {
          await removePanelReaction(fm.message, item.emoji, { client });
        }
      });
    });
    logger.info(`reaction-roles: pruned ${items.length} item(s) for deleted role ${roleId} in ${guild.id}`);
    return items.length;
  } catch (err) {
    logger.error(`reaction-roles: could not prune role ${roleId} in ${guild?.id}`, err);
    return 0;
  }
}

// ── Member-facing role changes ────────────────────────────────────────────────

const mentions = (ids) => joinWithLimit(ids.map((id) => `<@&${id}>`), { max: 900 });
const roleReason = (group) => clampText(`Reaction role: ${group.title}`, 512);

const FAIL_TEXT = Object.freeze({ 50013: "I'm missing permission to manage it", 10011: 'it no longer exists' });

/**
 * Checks, applies and reports one plan. Returns the member-facing text plus what changed.
 * @returns {Promise<{ content: string, res: { added: string[], removed: string[] } }>}
 */
async function runPlan({ guild, member, group, plan, client, targetRoleIds = [] }) {
  const nothing = { added: [], removed: [] };
  if (plan.reason) {
    let content;
    if (plan.reason === 'max') content = `You can hold at most **${group.maxRoles}** role(s) from this panel. Remove one first.`;
    else if (plan.reason === 'already') {
      content = targetRoleIds.length === 1 ? `You already have <@&${targetRoleIds[0]}>.` : 'You already have these roles.';
    } else if (plan.reason === 'not-held') {
      content = targetRoleIds.length ? `You don't have ${mentions(targetRoleIds)}, so there's nothing to remove.` : "You don't have those roles, so there's nothing to remove.";
    } else content = 'No changes: your roles already match your choice.';
    return { content, res: nothing };
  }

  const v = await verifyRoles(guild, [...plan.toAdd, ...plan.toRemove]);
  for (const id of v.gone) runInBackground('prune', () => pruneItemsForRole(guild, id, { client }));
  const lost = new Set([...v.gone, ...v.unverified]);
  if (plan.toAdd.some((id) => v.gone.includes(id))) {
    return { content: 'That role no longer exists, so I removed it from this panel.', res: nothing };
  }
  if (plan.toAdd.some((id) => lost.has(id))) {
    return { content: "I couldn't check that role right now. Please try again in a moment.", res: nothing };
  }
  const usable = { ...plan, toRemove: plan.toRemove.filter((id) => !lost.has(id)) };
  if (v.blocked.length) {
    return {
      content: v.noManageRoles
        ? 'I need the **Manage Roles** permission to give roles. Ask an admin to fix my permissions.'
        : `I can't manage ${mentions(v.blocked)} because it's above my highest role. Ask an admin to move my role higher.`,
      res: nothing,
    };
  }
  if (usable.toAdd.length === 0 && usable.toRemove.length === 0) {
    return { content: 'No changes: your roles already match your choice.', res: nothing };
  }

  const res = await applyRoleChange(member, usable, { reason: roleReason(group) });
  const lines = [];
  if (res.added.length) lines.push(`✅ Added: ${mentions(res.added)}`);
  if (res.removed.length) lines.push(`➖ Removed: ${mentions(res.removed)}`);
  if (plan.skipped.length) lines.push(`⚠️ Not added (limit of **${group.maxRoles}**): ${mentions(plan.skipped)}`);
  if (res.failed) {
    const why = FAIL_TEXT[res.failed.code] ?? 'something went wrong';
    lines.push(`⚠️ I couldn't ${res.failed.op} <@&${res.failed.roleId}>: ${why}.`);
    if (res.failed.code === 10011) {
      const verdict = await roleVerdict(guild, res.failed.roleId);
      if (verdict === 'gone') runInBackground('prune', () => pruneItemsForRole(guild, res.failed.roleId, { client }));
    }
  }
  if (lines.length === 0) lines.push('No changes: your roles already match your choice.');
  return { content: clampText(lines.join('\n'), 2000), res };
}

/**
 * Checks, applies and reports one plan for a member.
 * @returns {Promise<string>} the member-facing text
 */
export async function executePlan(args) {
  return (await runPlan(args)).content;
}

async function overRateLimit(guildId, userId) {
  // Redis down (null) -> fail OPEN: allow the change.
  const n = await cache.slidingWindow(`rr:rate:${guildId}:${userId}`, RATE_LIMIT.windowMs);
  return n !== null && n > RATE_LIMIT.max;
}

const RATE_MESSAGE = "You're changing roles too quickly. Wait a few seconds and try again.";
const NO_PANEL = '⚠️ This panel is no longer active.';
const NO_OPTION = '⚠️ That option is no longer on this panel.';
const OOPS = 'Something went wrong while updating your roles. Please try again.';
const NO_MENTIONS = Object.freeze({ parse: [] });

/** A forced REST read of the member (the interaction's snapshot can be stale); falls back to the snapshot. */
async function refreshMember(member) {
  try {
    return (await member.fetch(true)) ?? member;
  } catch {
    return member;
  }
}

/** Handles `rr:{groupId}:{roleId}` (BUTTONS panels, also the ids of panels posted before this rewrite). */
export async function handleRoleButton(interaction, client) {
  try {
    const { params: [groupId, roleId] } = parseCustomId(interaction.customId);
    if (!(await safeDefer(interaction, { ephemeral: true }))) return;
    const say = (content) => safeReply(interaction, { content, allowedMentions: NO_MENTIONS });

    const guild = interaction.guild ?? client?.guilds?.cache?.get(interaction.guildId) ?? null;
    if (!guild || !interaction.member) { await say(OOPS); return; }
    const group = groupId && roleId ? await getPanel(interaction.guildId, groupId) : null;
    if (!group) { await say(NO_PANEL); return; }
    const panelRoleIds = group.items.map((i) => i.roleId);
    if (!panelRoleIds.includes(roleId)) { await say(NO_OPTION); return; }

    const content = await memberMutex.run(`${interaction.guildId}:${interaction.user.id}`, async () => {
      if (await overRateLimit(interaction.guildId, interaction.user.id)) return RATE_MESSAGE;
      // A forced REST read inside the lock: single-id role calls do not update the cached member, so a stale snapshot
      // would report "Added" again on a fast second click.
      const member = await refreshMember(interaction.member);
      const plan = planRoleChange({
        type: group.type,
        source: 'button',
        panelRoleIds,
        heldRoleIds: panelRoleIds.filter((id) => member.roles.cache.has(id)),
        targetRoleIds: [roleId],
        maxRoles: group.maxRoles,
      });
      return executePlan({ guild, member, group, plan, client, targetRoleIds: [roleId] });
    });
    await say(content);
  } catch (err) {
    logger.error('reaction-roles button failed', err);
    await safeReply(interaction, { content: OOPS, ephemeral: true, allowedMentions: NO_MENTIONS });
  }
}

/** Handles `rr-select:{groupId}`: the shared dropdown on the panel, and the personal copy sent after each submission. */
export async function handleRoleSelect(interaction, client) {
  let personal = false;
  const reply = async (payload) => {
    if (!personal) return safeReply(interaction, { allowedMentions: NO_MENTIONS, ...payload });
    // After deferUpdate, safeReply would send a follow-up; editing the personal message is what we want.
    try {
      return await interaction.editReply({ allowedMentions: NO_MENTIONS, ...payload });
    } catch (err) {
      if (!isInteractionGone(err)) logger.warn('reaction-roles personal menu update failed', err);
      return null;
    }
  };
  try {
    personal = interaction.message?.flags?.has(MessageFlags.Ephemeral) === true;
    if (personal) {
      try {
        await interaction.deferUpdate();
      } catch (err) {
        if (isInteractionGone(err)) return;
        throw err;
      }
    } else if (!(await safeDefer(interaction, { ephemeral: true }))) {
      return;
    }

    const { params: [groupId] } = parseCustomId(interaction.customId);
    const guild = interaction.guild ?? client?.guilds?.cache?.get(interaction.guildId) ?? null;
    if (!guild || !interaction.member) { await reply({ content: OOPS, components: [] }); return; }
    const group = groupId ? await getPanel(interaction.guildId, groupId) : null;
    if (!group) { await reply({ content: NO_PANEL, components: [] }); return; }
    const panelRoleIds = group.items.map((i) => i.roleId);
    const submitted = interaction.values ?? [];
    const values = submitted.filter((v) => panelRoleIds.includes(v));
    if (submitted.length > 0 && values.length === 0) { await reply({ content: NO_OPTION, components: [] }); return; }

    const out = await memberMutex.run(`${interaction.guildId}:${interaction.user.id}`, async () => {
      if (await overRateLimit(interaction.guildId, interaction.user.id)) return { content: RATE_MESSAGE, held: null, res: null };
      const member = await refreshMember(interaction.member);
      const held = panelRoleIds.filter((id) => member.roles.cache.has(id));
      const plan = planRoleChange({
        type: group.type, source: 'select', panelRoleIds, heldRoleIds: held, targetRoleIds: values, maxRoles: group.maxRoles,
      });
      const { content, res } = await runPlan({ guild, member, group, plan, client, targetRoleIds: values });
      return { content, held, res };
    });

    let content = out.content;
    let components = [];
    if (out.held && (group.type === 'NORMAL' || group.type === 'UNIQUE')) {
      const heldAfter = panelRoleIds.filter((id) => (out.held.includes(id) && !out.res.removed.includes(id)) || out.res.added.includes(id));
      const row = buildSelectRow(group, { selectedRoleIds: heldAfter, placeholder: 'Adjust your roles from this panel' });
      if (row) {
        components = [row];
        content += '\n-# Your current roles from this panel are selected below. Change them there.';
      }
    }
    await reply({ content, components });
  } catch (err) {
    logger.error('reaction-roles select failed', err);
    await reply({ content: OOPS, components: [] });
  }
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

/** Sync and cheap: does this event concern an emoji-reaction panel (or might it, while the index is unknown)? */
export function reactionHookFilter(ctx) {
  if (!ctx.guildId || !ctx.message?.id) return false;
  const p = peekPanelIndex(ctx.guildId);
  if (!p || !p.fresh) return true; // run() loads the index
  return p.byMessageId.get(ctx.message.id)?.layout === 'REACTIONS';
}

export function messageDeleteFilter(ctx) {
  if (!ctx.guildId || !ctx.message?.id) return false;
  const p = peekPanelIndex(ctx.guildId);
  if (!p || !p.fresh) return true;
  return p.byMessageId.has(ctx.message.id);
}

function startReactionWork(ctx, source) {
  void processReaction(ctx, source).catch((err) => logger.error('reaction-roles reaction failed', err));
}

/** The hook bodies return at once so starboard, achievements and automations are never held up. */
export function onReactionAdd(ctx) { startReactionWork(ctx, 'react'); }
export function onReactionRemove(ctx) { startReactionWork(ctx, 'unreact'); }

async function removeMemberReaction(msg, key, userId) {
  try {
    const reaction = msg?.reactions?.cache?.find((r) => reactionKey(r) === key);
    if (reaction) await reaction.users.remove(userId);
  } catch (err) {
    logger.debug(`reaction-roles: could not take back a reaction: ${err?.message ?? err}`);
  }
}

/**
 * The body of the reaction hooks. Exported for tests; resolves when the work is done.
 * @param {object} ctx hook context of messageReactionAdd / messageReactionRemove
 * @param {'react'|'unreact'} source
 */
export async function processReaction(ctx, source) {
  const index = await getPanelIndex(ctx.guildId);
  if (!index) return; // fail closed
  const panel = index.get(ctx.message.id);
  if (panel?.layout !== 'REACTIONS') return;
  const key = normalizeEmojiKey(ctx.emojiKey);
  const item = panel.items.find((i) => i.key === key);
  if (!item) return; // foreign reactions are left alone
  const messageId = ctx.message.id;
  const userId = ctx.user.id;
  if (source === 'unreact' && consumeReactionSuppression(messageId, userId, key)) return;
  const guild = ctx.guild;

  await memberMutex.run(`${ctx.guildId}:${userId}`, async () => {
    if (await overRateLimit(ctx.guildId, userId)) {
      logger.debug(`reaction-roles: rate limit hit for ${userId} in ${ctx.guildId}`);
      return;
    }
    let member = await ctx.getMember();
    if (!member) return; // the user left
    member = await refreshMember(member); // fresh roles inside the lock

    const panelRoleIds = panel.items.map((i) => i.roleId);
    const plan = planRoleChange({
      type: panel.type,
      source,
      panelRoleIds,
      heldRoleIds: panelRoleIds.filter((id) => member.roles.cache.has(id)),
      targetRoleIds: [item.roleId],
      maxRoles: panel.maxRoles,
    });

    if (plan.reason === 'max') {
      // At the cap: take the reaction back (nothing is posted or DMed).
      suppressReactionRemoval(messageId, userId, key);
      if (hasManageMessages(guild, panel.channelId, ctx.message.channel)) {
        await removeMemberReaction(await ctx.ensureMessage(), key, userId);
      } else if (warnThrottle(`${panel.id}:manage-messages`)) {
        logger.warn(`reaction-roles: panel ${panel.id} in ${ctx.guildId} needs Manage Messages to take back reactions over the role limit`);
      }
      return;
    }
    if (plan.reason) return;

    const v = await verifyRoles(guild, [...plan.toAdd, ...plan.toRemove]);
    for (const id of v.gone) runInBackground('prune', () => pruneItemsForRole(guild, id, { client: ctx.client }));
    const lost = new Set([...v.gone, ...v.unverified]);
    if (plan.toAdd.some((id) => lost.has(id))) return;
    const usable = { ...plan, toRemove: plan.toRemove.filter((id) => !lost.has(id)) };
    if (v.blocked.length) {
      if (warnThrottle(`${panel.id}:blocked`)) {
        logger.warn(`reaction-roles: cannot manage role(s) ${v.blocked.join(',')} for panel ${panel.id} in ${ctx.guildId} (${v.noManageRoles ? 'no Manage Roles' : 'above my highest role'})`);
      }
      return;
    }
    if (usable.toAdd.length === 0 && usable.toRemove.length === 0) return;

    const res = await applyRoleChange(member, usable, { reason: roleReason(panel) });
    if (res.failed) {
      if (res.failed.code === 10011) {
        if ((await roleVerdict(guild, res.failed.roleId)) === 'gone') runInBackground('prune', () => pruneItemsForRole(guild, res.failed.roleId, { client: ctx.client }));
      } else if (warnThrottle(`${panel.id}:failed`)) {
        logger.warn(`reaction-roles: could not ${res.failed.op} role ${res.failed.roleId} for panel ${panel.id} in ${ctx.guildId} (code ${res.failed.code})`);
      }
    }

    // UNIQUE: the roles that were swapped out also lose their reactions.
    if (panel.type === 'UNIQUE' && source === 'react' && res.removed.length) {
      let msg = null;
      for (const roleId of res.removed) {
        const other = panel.items.find((i) => i.roleId === roleId && i.key);
        if (!other) continue;
        if (!hasManageMessages(guild, panel.channelId, ctx.message.channel)) {
          if (warnThrottle(`${panel.id}:manage-messages`)) {
            logger.warn(`reaction-roles: panel ${panel.id} in ${ctx.guildId} needs Manage Messages to remove swapped-out reactions`);
          }
          break;
        }
        suppressReactionRemoval(messageId, userId, other.key);
        msg ??= await ctx.ensureMessage();
        await removeMemberReaction(msg, other.key, userId);
      }
    }
  });
}

/** After a moderator clears every reaction, the bot puts its own back (roles never change). */
export function onReactionRemoveAll(ctx) {
  runInBackground('restore reactions', async () => {
    const panel = (await getPanelIndex(ctx.guildId))?.get(ctx.message.id);
    if (panel?.layout !== 'REACTIONS' || !restoreThrottle(ctx.message.id)) return;
    const msg = await ctx.ensureMessage();
    if (msg) await ensurePanelReactions(msg, panel.items);
  });
}

/** After one emoji is cleared from a panel message, the bot puts its own reaction back when that emoji is an item. */
export function onReactionRemoveEmoji(ctx) {
  runInBackground('restore reaction', async () => {
    const panel = (await getPanelIndex(ctx.guildId))?.get(ctx.message.id);
    if (panel?.layout !== 'REACTIONS') return;
    const key = normalizeEmojiKey(ctx.emojiKey);
    const item = panel.items.find((i) => i.key === key);
    if (!item) return;
    const msg = await ctx.ensureMessage();
    if (msg) await addPanelReaction(msg, item.emoji);
  });
}

/** A panel message was deleted: flag the panel so /reactionrole list can say so and repost can restore it. */
export async function onMessageDelete(ctx) {
  const reason = wasDeletedByBot(ctx.message.id);
  if (typeof reason === 'string' && reason.startsWith('reaction-roles:')) return; // our own delete / repost
  const panel = (await getPanelIndex(ctx.guildId))?.get(ctx.message.id);
  if (!panel) return;
  logger.warn(`reaction-roles: panel "${clampText(panel.title, 80)}" (${panel.id}) in guild ${ctx.guildId} lost its message ${ctx.message.id}. An admin can run /reactionrole repost.`);
  if (ctx.guild?.available === false) return; // rule 20
  try {
    await prisma.reactionRoleGroup.updateMany({
      where: { guildId: ctx.guildId, messageId: ctx.message.id },
      data: { messageMissingAt: new Date() },
    });
  } catch (err) {
    logger.error(`reaction-roles: could not flag the panels of message ${ctx.message.id}`, err);
  }
}

/** A role was deleted: drop its items and re-render the panels. */
export async function onRoleDelete(ctx) {
  await pruneItemsForRole(ctx.guild, ctx.role.id, { client: ctx.client });
}
