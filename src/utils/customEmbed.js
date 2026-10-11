// ── Message templates ──────────────────────────────────────────────────────────
// The model for admin-configured messages (welcome, reminders, tickets, ...): a MessageTemplate
// is { content, embed, buttons } with placeholders, validated against Discord's limits and
// rendered into a message payload. Rendering builds plain JSON that is clamped and URL-checked
// first and wrapped afterwards; it never uses discord.js's validating setters, because those
// throw on an over-long title or an unresolved '{user.avatar}' URL. Nothing here throws.
import {
  ActionRowBuilder,
  ButtonStyle,
  EmbedBuilder,
  ModalBuilder,
  TextInputBuilder,
  TextInputStyle,
} from 'discord.js';
import { COLORS } from '../config/constants.js';
import logger from './logger.js';
import { renderPlaceholders } from './placeholderParser.js';
import { clampText } from './text.js';

/**
 * @typedef {{ title?: string|null, description?: string|null, url?: string|null, color?: number|null,
 *   author?: { name: string, iconUrl?: string|null, url?: string|null }|null,
 *   footer?: { text: string, iconUrl?: string|null }|null,
 *   thumbnail?: string|null, image?: string|null,
 *   fields?: { name: string, value: string, inline?: boolean }[], timestamp?: boolean }} EmbedSpec
 * @typedef {{ label: string, url: string, emoji?: string|null }} LinkButtonSpec
 * @typedef {{ content?: string|null, embed?: EmbedSpec|null, buttons?: LinkButtonSpec[] }} MessageTemplate
 */

/** Embed and message limits. Per-guild entity caps are a different table (config LIMITS). */
export const LIMITS = Object.freeze({
  content: 2000, title: 256, description: 4096, fieldName: 256, fieldValue: 1024,
  fields: 25, footer: 2048, author: 256, total: 6000, buttons: 25, buttonLabel: 80, url: 2048, buttonUrl: 512,
});

/** Modal input ids (`kind: 'field'`): they carry no parameters and have no handler. */
export const TEMPLATE_FIELD_IDS = Object.freeze({
  content: 'tpl-content', title: 'tpl-title', description: 'tpl-description', color: 'tpl-color', footer: 'tpl-footer',
});

/** Maximum length of each modal input. Discord rejects a modal whose prefill is longer. */
export const MODAL_INPUT_MAX = Object.freeze({ content: 2000, title: 256, description: 4000, color: 16, footer: 2048 });

const MAX_JSON_BYTES = 20_480;
const BUTTONS_PER_ROW = 5;
const MAX_ROWS = 5;

// ── Small helpers ──────────────────────────────────────────────────────────────

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const isInt24 = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffff;

/**
 * True for an absolute http(s) URL of at most 2048 characters without whitespace.
 * @param {unknown} str
 * @returns {boolean}
 */
export function isHttpUrl(str) {
  if (typeof str !== 'string' || str.length > LIMITS.url || /\s/.test(str)) return false;
  try {
    const u = new URL(str);
    return (u.protocol === 'http:' || u.protocol === 'https:') && u.hostname !== '';
  } catch {
    return false;
  }
}

const ATTACHMENT_RE = /^attachment:\/\/[^\s/]{1,200}$/;
const imageUrlOk = (s) => isHttpUrl(s) || ATTACHMENT_RE.test(s);
const isPlaceholderUrl = (s) => typeof s === 'string' && s.includes('{');

// A string or finite number with content; anything else is absent.
function text(v) {
  if (typeof v === 'number' && Number.isFinite(v)) return String(v);
  return typeof v === 'string' && v.trim() !== '' ? v : null;
}

const CUSTOM_EMOJI_RE = /^<(a?):(\w{2,32}):(\d{17,20})>$/;
const UNICODE_EMOJI_RE = /^(?:\p{RGI_Emoji}|\p{Extended_Pictographic}️?)$/v;

// Discord button emoji: `{ id, name, animated }` for a custom emoji, `{ name }` for a single
// Unicode emoji, null for anything else (discord.js's own parser accepts any text as a name).
function parseButtonEmoji(str) {
  if (typeof str !== 'string') return null;
  const s = str.trim();
  const custom = CUSTOM_EMOJI_RE.exec(s);
  if (custom) return { id: custom[3], name: custom[2], animated: custom[1] === 'a' };
  if (s.length <= 16 && UNICODE_EMOJI_RE.test(s)) return { name: s };
  return null;
}

// ── Colours ────────────────────────────────────────────────────────────────────

const COLOR_NAMES = Object.freeze({
  red: 0xed4245, green: 0x57f287, blue: 0x3498db, blurple: 0x5865f2, yellow: 0xfee75c,
  orange: 0xe67e22, purple: 0x9b59b6, pink: 0xeb459e, white: 0xffffff,
  black: 0x010101, // Discord renders 0 as "no colour"
  grey: 0x95a5a6, gray: 0x95a5a6,
});

/**
 * Parses '#5865F2', '5865f2', '0x5865F2', '#fff' (expanded), a colour name, or an integer.
 * @param {unknown} input
 * @returns {number|null}
 */
export function parseColor(input) {
  if (typeof input === 'number') return isInt24(input) ? input : null;
  if (typeof input !== 'string') return null;
  const s = input.trim().toLowerCase();
  if (Object.hasOwn(COLOR_NAMES, s)) return COLOR_NAMES[s];
  const m = /^(?:#|0x)?([0-9a-f]{3}|[0-9a-f]{6})$/.exec(s);
  if (!m) return null;
  const hex = m[1].length === 3 ? [...m[1]].map((c) => c + c).join('') : m[1];
  return parseInt(hex, 16);
}

/**
 * @param {number} n
 * @returns {string} '#RRGGBB' ('#000000' for anything that is not a colour)
 */
export function formatColor(n) {
  return isInt24(n) ? `#${n.toString(16).padStart(6, '0').toUpperCase()}` : '#000000';
}

// ── Normalisation ──────────────────────────────────────────────────────────────

/** @returns {MessageTemplate} a new empty template */
export function emptyTemplate() {
  return { content: null, embed: null, buttons: [] };
}

const iconOf = (o) => {
  const v = o?.iconUrl ?? o?.iconURL ?? o?.icon_url;
  return typeof v === 'string' ? v : null;
};
const strOrNull = (v) => (typeof v === 'string' ? v : null);

/**
 * Normalises an embed in any known shape (our EmbedSpec, an APIEmbed, or the legacy welcome
 * shape) into an EmbedSpec. Unknown keys are dropped; the field count is not truncated so
 * validateTemplate can report it.
 * @param {unknown} legacy
 * @returns {EmbedSpec}
 */
export function normalizeLegacyEmbed(legacy) {
  try {
    if (!isObject(legacy)) return {};
    const out = {};
    for (const key of ['title', 'description', 'url']) {
      const v = text(legacy[key]);
      if (v !== null) out[key] = v;
    }
    const color = parseColor(legacy.color);
    if (color !== null) out.color = color;

    const a = legacy.author;
    if (typeof a === 'string' && a.trim()) out.author = { name: a };
    else if (isObject(a) && typeof a.name === 'string' && a.name.trim()) {
      out.author = { name: a.name, iconUrl: iconOf(a), url: strOrNull(a.url) };
    }

    const f = legacy.footer;
    if (typeof f === 'string' && f.trim()) out.footer = { text: f, iconUrl: null };
    else if (isObject(f) && typeof f.text === 'string' && f.text.trim()) out.footer = { text: f.text, iconUrl: iconOf(f) };

    let thumb = legacy.thumbnail;
    if (isObject(thumb)) thumb = thumb.url;
    if (typeof thumb === 'string') {
      if (thumb === 'user_avatar') out.thumbnail = '{user.avatar}';
      else if (thumb === 'server_icon') out.thumbnail = '{server.icon}';
      else if (thumb !== 'none' && thumb.trim()) out.thumbnail = thumb;
    }

    let image = legacy.image;
    if (isObject(image)) image = image.url;
    if (typeof image === 'string' && image.trim()) out.image = image;

    if (Array.isArray(legacy.fields)) {
      const fields = [];
      for (const fld of legacy.fields) {
        if (!isObject(fld)) continue;
        const name = text(fld.name);
        const value = text(fld.value);
        if (name !== null && value !== null) fields.push({ name, value, inline: Boolean(fld.inline) });
      }
      if (fields.length) out.fields = fields;
    }

    const ts = legacy.timestamp;
    if (ts === true || (typeof ts === 'string' && ts !== '') || (typeof ts === 'number' && Number.isFinite(ts))) {
      out.timestamp = true;
    }
    return out;
  } catch {
    return {};
  }
}

// An embed is visible when Discord would draw something: a colour alone is not visible.
function isVisible(spec) {
  return Boolean(spec && (
    spec.title || spec.description || spec.fields?.length || spec.author?.name
    || spec.footer?.text || spec.image || spec.thumbnail
  ));
}

/**
 * Converts an Embed, an EmbedBuilder or an APIEmbed into an EmbedSpec.
 * @param {unknown} embed
 * @returns {EmbedSpec}
 */
export function specFromEmbed(embed) {
  try {
    const d = embed?.data ?? (typeof embed?.toJSON === 'function' ? embed.toJSON() : embed);
    if (!isObject(d)) return {};
    return normalizeLegacyEmbed({
      title: d.title,
      description: d.description,
      url: d.url,
      color: d.color,
      author: d.author && { name: d.author.name, iconUrl: d.author.icon_url, url: d.author.url },
      footer: d.footer && { text: d.footer.text, iconUrl: d.footer.icon_url },
      thumbnail: d.thumbnail?.url,
      image: d.image?.url,
      fields: d.fields,
      timestamp: !!d.timestamp,
    });
  } catch {
    return {};
  }
}

function buttonEmoji(emoji) {
  if (typeof emoji === 'string' && emoji.trim()) return emoji.trim();
  if (emoji?.id) return `<${emoji.animated ? 'a' : ''}:${emoji.name}:${emoji.id}>`;
  return typeof emoji?.name === 'string' && emoji.name ? emoji.name : null;
}

function normalizeButtons(input) {
  let raw = [];
  if (Array.isArray(input.buttons)) {
    raw = input.buttons;
  } else if (Array.isArray(input.components)) {
    for (const row of input.components) {
      for (const c of Array.isArray(row?.components) ? row.components : []) {
        if (c?.type === 2 && c?.style === 5) raw.push(c);
      }
    }
  }
  const out = [];
  for (const b of raw) {
    if (!isObject(b)) continue;
    const label = b.label === undefined || b.label === null ? '' : String(b.label).trim();
    const url = b.url === undefined || b.url === null ? '' : String(b.url).trim();
    if (label && url) out.push({ label, url, emoji: buttonEmoji(b.emoji) });
  }
  return out;
}

/**
 * Normalises anything a template might be stored or imported as into a MessageTemplate:
 * a template, a bare EmbedSpec, the legacy welcome shape, a plain string (content), or
 * Discord/Discohook message JSON. A colour-only embed is kept so a modal edit keeps its colour.
 * @param {unknown} input
 * @returns {MessageTemplate}
 */
export function normalizeTemplate(input) {
  try {
    if (typeof input === 'string') return { content: input.trim() ? input : null, embed: null, buttons: [] };
    if (!isObject(input)) return emptyTemplate();

    const isTemplate = ['content', 'embed', 'buttons', 'embeds', 'components'].some((k) => Object.hasOwn(input, k));
    if (!isTemplate) {
      const spec = normalizeLegacyEmbed(input);
      return { content: null, embed: isVisible(spec) ? spec : null, buttons: [] };
    }

    let spec = null;
    if (isObject(input.embed)) spec = normalizeLegacyEmbed(input.embed);
    else if (Array.isArray(input.embeds) && input.embeds[0]) spec = specFromEmbed(input.embeds[0]);
    if (spec && !isVisible(spec) && !Number.isInteger(spec.color)) spec = null;

    return { content: text(input.content), embed: spec, buttons: normalizeButtons(input) };
  } catch {
    return emptyTemplate();
  }
}

// ── Validation ─────────────────────────────────────────────────────────────────

const URL_CHECKS = [
  ['Embed URL', (e) => e.url, false],
  ['Author URL', (e) => e.author?.url, false],
  ['Author icon', (e) => e.author?.iconUrl, true],
  ['Footer icon', (e) => e.footer?.iconUrl, true],
  ['Thumbnail', (e) => e.thumbnail, true],
  ['Image', (e) => e.image, true],
];

function embedTextLength(e) {
  let n = (e.title?.length ?? 0) + (e.description?.length ?? 0) + (e.author?.name?.length ?? 0) + (e.footer?.text?.length ?? 0);
  for (const f of e.fields ?? []) n += (f.name?.length ?? 0) + (f.value?.length ?? 0);
  return n;
}

/**
 * The error validateTemplate reports for a template with no text, no visible embed and no buttons. Compare
 * against this constant (or pass `requireSomething: false`) instead of matching the wording.
 */
export const EMPTY_MESSAGE_ERROR = 'The message is empty. Add some text or an embed.';

/**
 * Checks a template's raw (pre-render) lengths and URLs against Discord's limits.
 * @param {unknown} tpl
 * @param {{ requireSomething?: boolean }} [opts] - reject an empty template (default true)
 * @returns {{ ok: boolean, errors: string[] }}
 */
export function validateTemplate(tpl, { requireSomething = true } = {}) {
  const errors = [];
  try {
    const t = normalizeTemplate(tpl);

    if (t.content && t.content.length > LIMITS.content) {
      errors.push(`Message content is ${t.content.length} characters (max ${LIMITS.content}).`);
    }

    const e = t.embed;
    if (e) {
      if (e.title && e.title.length > LIMITS.title) errors.push(`Title is ${e.title.length} characters (max ${LIMITS.title}).`);
      if (e.description && e.description.length > LIMITS.description) {
        errors.push(`Description is ${e.description.length} characters (max ${LIMITS.description}).`);
      }
      if (e.author?.name && e.author.name.length > LIMITS.author) {
        errors.push(`Author name is ${e.author.name.length} characters (max ${LIMITS.author}).`);
      }
      if (e.footer?.text && e.footer.text.length > LIMITS.footer) {
        errors.push(`Footer is ${e.footer.text.length} characters (max ${LIMITS.footer}).`);
      }
      const fields = e.fields ?? [];
      if (fields.length > LIMITS.fields) errors.push(`Embed has ${fields.length} fields (max ${LIMITS.fields}).`);
      fields.forEach((f, idx) => {
        if (f.name.length > LIMITS.fieldName) errors.push(`Field ${idx + 1} name is ${f.name.length} characters (max ${LIMITS.fieldName}).`);
        if (f.value.length > LIMITS.fieldValue) errors.push(`Field ${idx + 1} value is ${f.value.length} characters (max ${LIMITS.fieldValue}).`);
      });
      const total = embedTextLength(e);
      if (total > LIMITS.total) errors.push(`Embed text totals ${total} characters (max ${LIMITS.total}).`);
      if (e.color !== undefined && e.color !== null && !isInt24(e.color)) errors.push('Colour must be a hex value like #5865F2.');
      for (const [label, pick, imageLike] of URL_CHECKS) {
        const v = pick(e);
        if (v === undefined || v === null) continue;
        if (typeof v === 'string' && v.length > LIMITS.url) errors.push(`${label} is longer than ${LIMITS.url} characters.`);
        else if (!(isHttpUrl(v) || isPlaceholderUrl(v) || (imageLike && ATTACHMENT_RE.test(String(v))))) {
          errors.push(`${label} must be an http(s) URL.`);
        }
      }
    }

    if (t.buttons.length > LIMITS.buttons) errors.push(`Too many link buttons (${t.buttons.length}, max ${LIMITS.buttons}).`);
    t.buttons.forEach((b, idx) => {
      const i = idx + 1;
      if (!b.label) errors.push(`Button ${i} needs a label.`);
      else if (b.label.length > LIMITS.buttonLabel) errors.push(`Button ${i} label is ${b.label.length} characters (max ${LIMITS.buttonLabel}).`);
      if (b.url.length > LIMITS.buttonUrl) errors.push(`Button ${i} URL is ${b.url.length} characters (max ${LIMITS.buttonUrl}).`);
      else if (!(isHttpUrl(b.url) || isPlaceholderUrl(b.url))) errors.push(`Button ${i} URL must be an http(s) URL.`);
      if (b.emoji && !parseButtonEmoji(b.emoji)) errors.push(`Button ${i} emoji is not a valid emoji.`);
    });

    if (requireSomething && !t.content?.trim() && !isVisible(t.embed) && t.buttons.length === 0) {
      errors.push(EMPTY_MESSAGE_ERROR);
    }
  } catch (err) {
    logger.warn(`validateTemplate failed: ${err?.message}`);
    errors.push('The template could not be checked.');
  }
  return { ok: errors.length === 0, errors };
}

// ── Rendering ──────────────────────────────────────────────────────────────────

/**
 * Renders an EmbedSpec: placeholders expanded, every part clamped after expansion, URLs that
 * do not resolve to http(s) dropped, and the 6000-character total enforced (the description
 * shrinks first, then field values from the last field, then fields are dropped).
 * @param {EmbedSpec} spec
 * @param {import('./placeholderParser.js').RenderContext} renderCtx
 * @param {{ defaultColor?: number|null }} [opts] - used when the spec has no colour; null omits the colour
 * @returns {EmbedBuilder|null} null when nothing visible remains
 */
export function renderEmbed(spec, renderCtx, { defaultColor = COLORS.INFO } = {}) {
  try {
    const ctx = renderCtx ?? {};
    const s = normalizeLegacyEmbed(spec);
    const R = (t, max) => clampText(renderPlaceholders(t, ctx), max);
    const U = (u) => renderPlaceholders(u, ctx).trim();

    const data = {};
    const title = s.title ? R(s.title, LIMITS.title) : '';
    if (title) data.title = title;
    let description = s.description ? R(s.description, LIMITS.description) : '';
    if (s.url) { const u = U(s.url); if (isHttpUrl(u)) data.url = u; }

    let author = null;
    if (s.author?.name) {
      const name = R(s.author.name, LIMITS.author);
      if (name) {
        author = { name };
        if (s.author.iconUrl) { const u = U(s.author.iconUrl); if (imageUrlOk(u)) author.icon_url = u; }
        if (s.author.url) { const u = U(s.author.url); if (isHttpUrl(u)) author.url = u; }
      }
    }
    let footer = null;
    if (s.footer?.text) {
      const t = R(s.footer.text, LIMITS.footer);
      if (t) {
        footer = { text: t };
        if (s.footer.iconUrl) { const u = U(s.footer.iconUrl); if (imageUrlOk(u)) footer.icon_url = u; }
      }
    }
    if (s.thumbnail) { const u = U(s.thumbnail); if (imageUrlOk(u)) data.thumbnail = { url: u }; }
    if (s.image) { const u = U(s.image); if (imageUrlOk(u)) data.image = { url: u }; }

    const fields = [];
    for (const f of (s.fields ?? []).slice(0, LIMITS.fields)) {
      let name = R(f.name, LIMITS.fieldName);
      let value = R(f.value, LIMITS.fieldValue);
      if (!name && !value) continue;
      if (!name) name = '​';
      if (!value) value = '​';
      fields.push({ name, value, inline: Boolean(f.inline) });
    }

    // Total-length shrink: description first, then field values from the last, then drop fields.
    const total = () => title.length + description.length + (author?.name.length ?? 0) + (footer?.text.length ?? 0)
      + fields.reduce((n, f) => n + f.name.length + f.value.length, 0);
    let excess = total() - LIMITS.total;
    if (excess > 0 && description) {
      const newLen = description.length - excess;
      description = newLen >= 1 ? clampText(description, newLen) : '';
      excess = total() - LIMITS.total;
    }
    for (let i = fields.length - 1; i >= 0 && excess > 0; i--) {
      fields[i].value = clampText(fields[i].value, Math.max(1, fields[i].value.length - excess));
      excess = total() - LIMITS.total;
    }
    while (excess > 0 && fields.length) {
      fields.pop();
      excess = total() - LIMITS.total;
    }

    if (description) data.description = description;
    if (author) data.author = author;
    if (footer) data.footer = footer;
    if (fields.length) data.fields = fields;
    if (!data.title && !data.description && !data.fields && !data.author && !data.footer && !data.image && !data.thumbnail) {
      return null;
    }

    const color = Number.isInteger(s.color) ? s.color : defaultColor;
    if (color !== null && color !== undefined) data.color = color;
    if (s.timestamp) data.timestamp = new Date().toISOString();
    return new EmbedBuilder(data);
  } catch (err) {
    logger.warn(`renderEmbed failed: ${err?.message}`);
    return null;
  }
}

/**
 * Renders a template into a message payload. `embeds` and `components` are always present, so
 * passing the result to `message.edit` removes stale embeds and buttons. To also clear removed
 * text when editing, use `message.edit({ content: '', ...rendered })`.
 * Link buttons whose rendered URL is not http(s) (or is over 512 characters) are dropped, and
 * a button emoji is used only when it parses. The caller's `allowedMentions` is used verbatim.
 * @param {unknown} tpl
 * @param {import('./placeholderParser.js').RenderContext} renderCtx
 * @param {{ defaultColor?: number|null, allowedMentions?: object, extraComponents?: object[] }} [opts]
 *   `extraComponents` (e.g. rows of select menus) go after the link rows; link rows + extras never exceed 5 rows
 * @returns {{ content?: string, embeds: EmbedBuilder[], components: ActionRowBuilder[], allowedMentions: object }|null}
 *   null when there is no content, no visible embed and no components
 */
export function renderTemplate(tpl, renderCtx, {
  defaultColor, allowedMentions = { parse: ['users'] }, extraComponents = [],
} = {}) {
  try {
    const ctx = renderCtx ?? {};
    const t = normalizeTemplate(tpl);

    let content = '';
    try {
      if (t.content) content = clampText(renderPlaceholders(t.content, ctx), LIMITS.content);
      if (content.trim() === '') content = '';
    } catch (err) {
      logger.warn('renderTemplate: content skipped', err);
      content = '';
    }

    let embed = null;
    try {
      embed = t.embed ? renderEmbed(t.embed, ctx, { defaultColor: defaultColor ?? COLORS.INFO }) : null;
    } catch (err) {
      logger.warn('renderTemplate: embed skipped', err);
    }

    const extras = Array.isArray(extraComponents) ? extraComponents : [];
    const linkRows = [];
    try {
      const buttons = [];
      for (const b of t.buttons) {
        const label = clampText(renderPlaceholders(b.label, ctx).trim(), LIMITS.buttonLabel);
        const url = renderPlaceholders(b.url, ctx).trim();
        if (!label || !isHttpUrl(url) || url.length > LIMITS.buttonUrl) continue;
        const component = { type: 2, style: ButtonStyle.Link, label, url };
        const emoji = b.emoji ? parseButtonEmoji(b.emoji) : null;
        if (emoji) component.emoji = emoji;
        buttons.push(component);
      }
      const allowedRows = Math.max(0, MAX_ROWS - extras.length);
      for (let i = 0; i < buttons.length; i += BUTTONS_PER_ROW) linkRows.push(buttons.slice(i, i + BUTTONS_PER_ROW));
      if (linkRows.length > allowedRows) {
        logger.warn('Template has more link buttons than fit; extra buttons dropped');
        linkRows.length = allowedRows;
      }
    } catch (err) {
      logger.warn('renderTemplate: buttons skipped', err);
      linkRows.length = 0;
    }

    const components = [...linkRows.map((components) => new ActionRowBuilder({ type: 1, components })), ...extras];
    if (!content && !embed && components.length === 0) return null;
    return { ...(content ? { content } : {}), embeds: embed ? [embed] : [], components, allowedMentions };
  } catch (err) {
    logger.warn('renderTemplate failed', err);
    return null;
  }
}

// ── Modal ──────────────────────────────────────────────────────────────────────

const MODAL_INPUTS = [
  { key: 'content', label: 'Message text (outside the embed)', style: TextInputStyle.Paragraph, placeholder: 'Hey {user}, welcome to {server}!' },
  { key: 'title', label: 'Embed title', style: TextInputStyle.Short, placeholder: 'Welcome to {server}!' },
  { key: 'description', label: 'Embed description', style: TextInputStyle.Paragraph, placeholder: 'Use {user}, {server}, {server.member_count} …' },
  { key: 'color', label: 'Embed colour (hex or name)', style: TextInputStyle.Short, placeholder: '#5865F2' },
  { key: 'footer', label: 'Embed footer', style: TextInputStyle.Paragraph, placeholder: '{server.member_count} members' },
];

/**
 * Builds the edit modal: five optional inputs prefilled from the template.
 * Discord rejects a modal whose prefill is longer than the input's maximum, so by default
 * prefills are clamped. With `overflow: 'throw'` it throws a RangeError (its `.fields` lists the
 * over-long keys) so the caller can refuse instead of silently cutting text that would be lost on save.
 * @param {{ customId: string, title?: string, template?: unknown, overflow?: 'clamp'|'throw' }} opts
 * @returns {ModalBuilder}
 * @throws {RangeError} only with `overflow: 'throw'`
 */
export function buildTemplateModal({ customId, title, template, overflow = 'clamp' }) {
  const t = normalizeTemplate(template);
  const prefill = {
    content: t.content,
    title: t.embed?.title,
    description: t.embed?.description,
    color: Number.isInteger(t.embed?.color) && isInt24(t.embed.color) ? formatColor(t.embed.color) : null,
    footer: t.embed?.footer?.text,
  };
  if (overflow === 'throw') {
    const over = MODAL_INPUTS.map((i) => i.key).filter((k) => typeof prefill[k] === 'string' && prefill[k].length > MODAL_INPUT_MAX[k]);
    if (over.length) {
      throw Object.assign(new RangeError(`Too long for the form: ${over.join(', ')}.`), { fields: over });
    }
  }
  const modal = new ModalBuilder().setCustomId(customId).setTitle(clampText(title || 'Edit message', 45));
  for (const input of MODAL_INPUTS) {
    const field = new TextInputBuilder()
      .setCustomId(TEMPLATE_FIELD_IDS[input.key])
      .setLabel(input.label)
      .setStyle(input.style)
      .setMaxLength(MODAL_INPUT_MAX[input.key])
      .setRequired(false)
      .setPlaceholder(input.placeholder);
    const value = prefill[input.key];
    if (typeof value === 'string' && value !== '') field.setValue(value.slice(0, MODAL_INPUT_MAX[input.key]));
    modal.addComponents(new ActionRowBuilder().addComponents(field));
  }
  return modal;
}

const OUT_OF_DATE = 'This form is out of date. Open it again.';

/**
 * Merges a submitted template modal into a copy of `base`. An empty input clears that part;
 * a field that is missing altogether (a stale form) is an error and leaves that part unchanged.
 * The template is returned even when there are errors; the caller decides what to do with them.
 * An emptied form is an error (EMPTY_MESSAGE_ERROR) unless `requireSomething` is false (a caller that has a
 * default to fall back on, such as the welcome setup forms).
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 * @param {unknown} [base]
 * @param {{ requireSomething?: boolean }} [opts]
 * @returns {{ template: MessageTemplate, errors: string[] }}
 */
export function parseTemplateModal(interaction, base = emptyTemplate(), { requireSomething = true } = {}) {
  const errors = [];
  const baseTpl = normalizeTemplate(base);
  const tpl = structuredClone(baseTpl);
  const embed = tpl.embed ?? {};
  let stale = false;

  const get = (key) => {
    let v;
    try {
      v = interaction?.fields?.getTextInputValue(TEMPLATE_FIELD_IDS[key]);
    } catch {
      v = undefined;
    }
    if (typeof v !== 'string') { stale = true; return undefined; }
    return v;
  };

  const content = get('content');
  if (content !== undefined) tpl.content = content.trim() ? content : null;

  const titleV = get('title');
  if (titleV !== undefined) {
    if (titleV.trim()) embed.title = titleV; else delete embed.title;
  }

  const descV = get('description');
  if (descV !== undefined) {
    const original = baseTpl.embed?.description;
    // A long description that was only prefilled (cut to 4000) is not silently truncated on save.
    if (original && original.length > MODAL_INPUT_MAX.description && descV === original.slice(0, MODAL_INPUT_MAX.description)) {
      embed.description = original;
    } else if (descV.trim()) {
      embed.description = descV;
    } else {
      delete embed.description;
    }
  }

  const colorV = get('color');
  if (colorV !== undefined) {
    if (colorV.trim() === '') {
      delete embed.color;
    } else {
      const c = parseColor(colorV);
      if (c === null) errors.push('Colour must be a hex value like #5865F2 or a colour name.');
      else embed.color = c;
    }
  }

  const footerV = get('footer');
  if (footerV !== undefined) {
    if (footerV.trim()) embed.footer = { text: footerV, iconUrl: embed.footer?.iconUrl ?? null };
    else delete embed.footer;
  }

  if (stale) errors.unshift(OUT_OF_DATE);
  tpl.embed = isVisible(embed) || Number.isInteger(embed.color) ? embed : null;
  errors.push(...validateTemplate(tpl, { requireSomething }).errors);
  return { template: tpl, errors };
}

// ── JSON ───────────────────────────────────────────────────────────────────────

/**
 * @param {unknown} tpl
 * @returns {string} pretty JSON of the normalised template
 */
export function templateToJson(tpl) {
  return JSON.stringify(normalizeTemplate(tpl), null, 2);
}

/**
 * Imports a template from JSON: our own export, a bare EmbedSpec, the legacy welcome shape, or
 * Discord/Discohook message JSON. Rejects input over 20 KB.
 * @param {unknown} str
 * @returns {{ template: MessageTemplate|null, errors: string[] }}
 */
export function templateFromJson(str) {
  if (typeof str !== 'string') return { template: null, errors: ['No JSON was provided.'] };
  if (Buffer.byteLength(str, 'utf8') > MAX_JSON_BYTES) return { template: null, errors: ['The JSON is larger than 20 KB.'] };
  let parsed;
  try {
    parsed = JSON.parse(str);
  } catch (err) {
    return { template: null, errors: [`That is not valid JSON: ${clampText(err?.message, 200)}`] };
  }
  const template = normalizeTemplate(parsed);
  const { errors } = validateTemplate(template);
  return { template: errors.length ? null : template, errors };
}
