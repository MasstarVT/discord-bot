// Embed Messages service (pkg-embeds). Saved embeds (SavedEmbed rows holding a MessageTemplate), the pure template
// mutators behind every /embed subcommand, rendering, publish / update of the live message, the Welcome Channel
// presets, the `embed-modal` submit handler, the messageDelete hook body and the contracts other packages use:
//   getSavedEmbedTemplate(guildId, name)  -> MessageTemplate | null   (never throws; raw, placeholders unexpanded)
//   searchSavedEmbeds(guildId, query, take) -> [{ id, name, channelId, messageId }]  (never throws; [] on error)
//   normalizeEmbedName(input)             -> slug | null
// Redis (guild:{id}:embed:{name}) is a read accelerator for getSavedEmbedTemplate only; Postgres is the source of truth.
import {
  AttachmentBuilder,
  ChannelType,
  DiscordAPIError,
  OverwriteType,
  PermissionFlagsBits,
  RESTJSONErrorCodes,
} from 'discord.js';
import prisma from '../database/client.js';
import { cache } from './redis.js';
import { ensureGuildSettings, getSettingsSafe, peekSettings } from './settingsService.js';
import { COLORS, LIMITS as GUILD_LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { parseCustomId } from '../config/customIds.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { memberChannelProblem } from '../utils/privacyGuards.js';
import {
  emptyTemplate,
  isHttpUrl,
  normalizeTemplate,
  parseTemplateModal,
  renderTemplate,
  specFromEmbed,
  templateToJson,
  validateTemplate,
} from '../utils/customEmbed.js';
import { placeholderHelp, renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import {
  checkChannelUsable,
  isInteractionGone,
  missingBotPermissions,
  pingableRoleIds,
} from '../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { errorEmbed, successEmbed } from '../utils/embedBuilder.js';
import { ackBudgetMs, createKeyedMutex, withTimeout } from '../utils/async.js';
import { markBotDeletion } from '../utils/deletionTracker.js';
import logger from '../utils/logger.js';

// ── Constants ──────────────────────────────────────────────────────────────────

export const EMBED_NAME_MAX = 32;
export const EMBED_CACHE_TTL_SECONDS = 300;
export const EMBED_CACHE_MISS_TTL_SECONDS = 60;
export const IMPORT_MAX_BYTES = 20_480;
export const BUTTON_URL_MAX = 512;
export const PRESET_NAMES = Object.freeze(['welcome-greeting', 'welcome-rules', 'welcome-links', 'welcome-team']);
export const CLEAR_WORDS = Object.freeze(['none', 'clear', 'remove', '-']);

const MAX_FIELDS = 25;
const MAX_BUTTONS = 25;
const MAX_ROLE_MENTIONS = 100;
const MAX_UNKNOWN_TOKENS = 10;

// The only module-scope state: serialises every write, publish, update and delete of one embed in this process
// (all interactions of a guild arrive on its own shard, so this also covers two admins at once).
const embedLock = createKeyedMutex();

/**
 * A failure whose `.message` is shown to the admin verbatim. `code` is one of
 * 'INVALID' (default), 'NOTFOUND', 'TAKEN', 'LIMIT'. A rejected save also carries the rejected
 * `template` and the list of `errors`, so the caller can hand the text back as a draft file.
 */
export class EmbedEditError extends Error {
  constructor(message, code = 'INVALID', extra = {}) {
    super(message);
    this.name = 'EmbedEditError';
    this.code = code;
    Object.assign(this, extra);
  }
}

export const embedCacheKey = (guildId, name) => `guild:${guildId}:embed:${name}`;

const notFoundText = (name) => `There is no saved embed called **${name}**. See \`/embed list\`.`;
const takenText = (name) => `An embed called **${name}** already exists. Edit it with \`/embed edit name:${name}\`.`;
const limitText = () => `This server already has ${GUILD_LIMITS.SAVED_EMBEDS} saved embeds, the maximum. Delete one with \`/embed delete\` first.`;

/** @returns {string} the bullet list shown for a rejected save (at most 10 entries) */
export function formatErrors(errors) {
  return [...new Set(errors)].slice(0, 10).map((e) => `• ${e}`).join('\n');
}

const apiCode = (err) => (err instanceof DiscordAPIError ? err.code : null);

// ── Names ──────────────────────────────────────────────────────────────────────

/**
 * Normalises an embed name to a slug: lowercase, accents removed, spaces to '-', only a-z 0-9 - _, at most 32.
 * @param {unknown} input
 * @returns {string|null} null when nothing usable remains
 */
export function normalizeEmbedName(input) {
  if (typeof input !== 'string') return null;
  const s = input.normalize('NFKD').replace(/\p{M}+/gu, '').trim().toLowerCase()
    .replace(/\s+/g, '-').replace(/[^a-z0-9_-]/g, '').replace(/-{2,}/g, '-')
    .replace(/^-+/, '').slice(0, EMBED_NAME_MAX).replace(/-+$/, '');
  return s.length ? s : null;
}

/** @returns {boolean} true for `none`, `clear`, `remove` and `-` (any case) */
export function isClearWord(str) {
  return typeof str === 'string' && CLEAR_WORDS.includes(str.trim().toLowerCase());
}

// ── URL and emoji checks ───────────────────────────────────────────────────────

/**
 * An image URL: http(s), or a single placeholder such as {user.avatar} / {server.icon}.
 * @param {unknown} str
 * @returns {string} the trimmed value
 * @throws {EmbedEditError}
 */
export function checkImageUrl(str) {
  const v = typeof str === 'string' ? str.trim() : '';
  if (v.length > 0 && v.length <= 2048 && (isHttpUrl(v) || /^\{[a-z0-9_.]+\}$/i.test(v))) return v;
  throw new EmbedEditError('Images must be an http(s) link or a variable like {user.avatar} or {server.icon}.');
}

/**
 * A link: http(s) only, at most `max` characters.
 * @param {unknown} str
 * @param {number} [max=2048]
 * @returns {string} the trimmed value
 * @throws {EmbedEditError}
 */
export function checkLinkUrl(str, max = 2048) {
  const v = typeof str === 'string' ? str.trim() : '';
  if (v.length > 0 && v.length <= max && /^https?:\/\//i.test(v) && isHttpUrl(v)) return v;
  throw new EmbedEditError(`Links must start with http:// or https:// and be at most ${max} characters.`);
}

// Discord names custom emojis with 2-32 characters; {1,32} keeps the spec's `<:x:id>` examples valid here, and the
// foundation validator (validateTemplate) still refuses a one-character name when the button is saved.
const CUSTOM_EMOJI_RE = /^<(a?):([A-Za-z0-9_]{1,32}):(\d{17,20})>$/;
const UNICODE_EMOJI_RE = /^(?:\p{Extended_Pictographic}|\p{Regional_Indicator}{2}|[0-9#*]\u{FE0F}?⃣)[\u{FE0F}‍\p{Extended_Pictographic}\p{Emoji_Modifier}]*$/u;
const segmenter = new Intl.Segmenter('en', { granularity: 'grapheme' });

/**
 * True for a custom emoji (`<:name:id>` / `<a:name:id>`) or exactly one Unicode emoji.
 * @param {unknown} str
 * @returns {boolean}
 */
export function isEmojiSyntax(str) {
  if (typeof str !== 'string') return false;
  const s = str.trim();
  if (CUSTOM_EMOJI_RE.test(s)) return true;
  if (s.length === 0 || s.length > 32) return false;
  if ([...segmenter.segment(s)].length !== 1) return false;
  return UNICODE_EMOJI_RE.test(s);
}

/**
 * Validates a button emoji and returns it trimmed (ButtonBuilder.setEmoji parses the string).
 * @param {unknown} str
 * @param {(id: string) => boolean} [isKnownEmoji] - whether the bot can see a custom emoji id
 * @returns {string}
 * @throws {EmbedEditError}
 */
export function parseButtonEmoji(str, isKnownEmoji = () => true) {
  const s = typeof str === 'string' ? str.trim() : '';
  if (!isEmojiSyntax(s)) {
    throw new EmbedEditError("That isn't a valid emoji. Use a standard emoji or a custom emoji from a server I'm in.");
  }
  const custom = CUSTOM_EMOJI_RE.exec(s);
  if (custom && !isKnownEmoji(custom[3])) {
    throw new EmbedEditError("I can only use custom emojis from servers I'm in.");
  }
  return s;
}

// ── Validation ─────────────────────────────────────────────────────────────────

/**
 * Every write of a saved embed passes this: the foundation-utils checks (non-empty, Discord limits) plus the
 * link-button rules.
 * @param {unknown} tpl
 * @returns {string[]} problems; [] means the template may be saved
 */
export function validateForSave(tpl) {
  const errors = [...validateTemplate(tpl, { requireSomething: true }).errors];
  const t = normalizeTemplate(tpl);
  t.buttons.forEach((b, idx) => {
    const i = idx + 1;
    if (!b.label?.trim()) errors.push(`Button ${i} has no label.`);
    if (typeof b.url !== 'string' || b.url.length > BUTTON_URL_MAX || !/^https?:\/\//i.test(b.url) || !isHttpUrl(b.url)) {
      errors.push(`Button ${i} needs an http(s) link of at most ${BUTTON_URL_MAX} characters.`);
    }
    if (b.emoji && !isEmojiSyntax(b.emoji)) errors.push(`Button ${i} has an invalid emoji.`);
  });
  return [...new Set(errors)];
}

// ── Rendering ──────────────────────────────────────────────────────────────────

/** @returns {number} the guild's default embed colour, or the bot's info colour */
export function resolveDefaultColor(settings) {
  const c = settings?.embedColor;
  return Number.isInteger(c) && c >= 0 && c <= 0xFFFFFF ? c : COLORS.INFO;
}

/** @returns {string[]} the distinct `<@&id>` role ids in a message text (at most 100) */
export function extractRoleMentionIds(content) {
  return [...new Set([...String(content ?? '').matchAll(/<@&(\d{17,20})>/g)].map((m) => m[1]))].slice(0, MAX_ROLE_MENTIONS);
}

/**
 * Role-ping policy of a publish: user mentions ping, `@everyone`/`@here` never do, and a literal `<@&id>` role
 * mention pings only when the role is mentionable or `member` may mention every role in `channel`
 * (`pingableRoleIds`). `refused` lists the roles that will not be pinged.
 * @param {string|null|undefined} content
 * @param {import('discord.js').GuildBasedChannel} channel
 * @param {import('discord.js').GuildMember|null} member
 * @returns {{ allowedMentions: { parse: string[], roles: string[] }, refused: string[] }}
 */
export function resolvePublishMentions(content, channel, member) {
  const ids = extractRoleMentionIds(content);
  const { allowed, refused } = ids.length ? pingableRoleIds(channel, member, ids) : { allowed: [], refused: [] };
  return { allowedMentions: { parse: ['users'], roles: allowed }, refused };
}

/**
 * @param {string|null|undefined} content - the rendered message text
 * @param {'publish'|'none'} mode - 'none' (preview, update, presets) pings nobody
 * @param {{ channel?: object|null, member?: object|null }} [ctx] - required for 'publish'; without a channel no role is allowed
 * @returns {import('discord.js').MessageMentionOptions}
 */
export function mentionPolicy(content, mode, { channel = null, member = null } = {}) {
  if (mode !== 'publish') return { parse: [] };
  if (!channel) return { parse: ['users'], roles: [] }; // fail closed: nothing proves a role may be pinged
  return resolvePublishMentions(content, channel, member).allowedMentions;
}

/**
 * Renders a saved template into a message payload with the member running the command as {user}.
 * @param {unknown} tpl
 * @param {{ member?: object|null, guild?: object|null, channel?: object|null, settings?: object|null }} ctx
 * @param {{ mentions?: 'publish'|'none' }} [opts]
 * @returns {{ content?: string, embeds: object[], components: object[], allowedMentions: object }|null} null when empty
 */
export function renderSavedEmbed(tpl, { member, guild, channel, settings }, { mentions = 'none' } = {}) {
  const ctx = { member, user: member?.user ?? null, guild, channel };
  const out = renderTemplate(tpl, ctx, { defaultColor: resolveDefaultColor(settings) });
  if (!out) return null;
  return { ...out, allowedMentions: mentionPolicy(out.content, mentions, { channel, member }) };
}

/**
 * The distinct unknown {variables} in the text parts of a template (as typed, with braces), at most 10.
 * @param {unknown} tpl
 * @param {import('../utils/placeholderParser.js').RenderContext} renderCtx
 * @returns {string[]}
 */
export function collectUnknownTokens(tpl, renderCtx) {
  const t = normalizeTemplate(tpl);
  const strings = [
    t.content, t.embed?.title, t.embed?.description, t.embed?.author?.name, t.embed?.footer?.text,
    ...(t.embed?.fields ?? []).flatMap((f) => [f.name, f.value]),
    ...t.buttons.map((b) => b.label),
  ];
  const seen = new Set();
  for (const s of strings) {
    if (!s) continue;
    for (const token of renderPlaceholdersDetailed(s, renderCtx).unknownTokens) {
      seen.add(token);
      if (seen.size >= MAX_UNKNOWN_TOKENS) return [...seen];
    }
  }
  return [...seen];
}

/** @returns {string} the line appended to a change reply when the embed is published (else '') */
export function publishHint(row, name = row?.name) {
  if (!row?.messageId || !row?.channelId) return '';
  return `\nIt is published in <#${row.channelId}>. Run \`/embed update name:${name}\` to apply the change there.`;
}

/** @returns {string} the warning line for unknown variables (else '') */
export function unknownVariablesNote(tpl, renderCtx) {
  const tokens = collectUnknownTokens(tpl, renderCtx);
  return tokens.length ? `\n⚠️ Unknown variables are shown as typed: ${tokens.map((t) => `\`${t}\``).join(', ')}` : '';
}

/** @returns {AttachmentBuilder} a JSON file holding the template, so typed text is never lost on a rejected form */
export function draftAttachment(tpl) {
  return new AttachmentBuilder(Buffer.from(templateToJson(tpl), 'utf8'), { name: 'embed-draft.json' });
}

// ── Pure template mutators (1-based indexes; never mutate the argument) ────────

const cloneTemplate = (tpl) => structuredClone(normalizeTemplate(tpl));
const embedOf = (t) => { t.embed ??= {}; return t.embed; };
const clampIndex = (position, length) => Math.min(Math.max(Math.floor(position) - 1, 0), length);

function checkIndex(index, length, what) {
  if (!Number.isInteger(index) || index < 1 || index > length) throw new EmbedEditError(`There is no ${what} ${index}.`);
}

/** Adds a field (default: last; `position` is 1-based). @returns {object} a new template */
export function addField(tpl, { title, value, inline = false, position = null }) {
  const t = cloneTemplate(tpl);
  const name = typeof title === 'string' ? title.trim() : '';
  const text = typeof value === 'string' ? value.trim() : '';
  if (!name || !text) throw new EmbedEditError('Field title and text cannot be empty.');
  const embed = embedOf(t);
  const fields = (embed.fields ??= []);
  if (fields.length >= MAX_FIELDS) throw new EmbedEditError(`An embed can have at most ${MAX_FIELDS} fields.`);
  const at = position == null ? fields.length : clampIndex(position, fields.length);
  fields.splice(at, 0, { name, value: text, inline: Boolean(inline) });
  return t;
}

/** Changes a field; only the given parts change. @returns {object} a new template */
export function editField(tpl, index, { title, value, inline, position } = {}) {
  const t = cloneTemplate(tpl);
  const embed = embedOf(t);
  const fields = (embed.fields ??= []);
  checkIndex(index, fields.length, 'field');
  const field = fields[index - 1];
  if (title !== undefined) {
    if (!String(title).trim()) throw new EmbedEditError('Field title and text cannot be empty.');
    field.name = String(title).trim();
  }
  if (value !== undefined) {
    if (!String(value).trim()) throw new EmbedEditError('Field title and text cannot be empty.');
    field.value = String(value).trim();
  }
  if (inline !== undefined && inline !== null) field.inline = Boolean(inline);
  if (position != null) {
    fields.splice(index - 1, 1);
    fields.splice(clampIndex(position, fields.length), 0, field);
  }
  return t;
}

/** Removes a field. @returns {object} a new template */
export function removeField(tpl, index) {
  const t = cloneTemplate(tpl);
  const fields = t.embed?.fields ?? [];
  checkIndex(index, fields.length, 'field');
  fields.splice(index - 1, 1);
  return t;
}

/** Adds a link button (http(s) URL of at most 512 characters). @returns {object} a new template */
export function addButton(tpl, { label, url, emoji = null, position = null }) {
  const t = cloneTemplate(tpl);
  const text = typeof label === 'string' ? label.trim() : '';
  if (!text) throw new EmbedEditError('The button needs a label.');
  const link = checkLinkUrl(url, BUTTON_URL_MAX);
  if (t.buttons.length >= MAX_BUTTONS) throw new EmbedEditError(`A message can have at most ${MAX_BUTTONS} link buttons.`);
  const at = position == null ? t.buttons.length : clampIndex(position, t.buttons.length);
  t.buttons.splice(at, 0, { label: text, url: link, emoji: emoji ?? null });
  return t;
}

/** Removes a link button. @returns {object} a new template */
export function removeButton(tpl, index) {
  const t = cloneTemplate(tpl);
  checkIndex(index, t.buttons.length, 'button');
  t.buttons.splice(index - 1, 1);
  return t;
}

/** Sets the large image and/or thumbnail: undefined keeps it, null clears it. @returns {object} a new template */
export function setImages(tpl, { image, thumbnail } = {}) {
  const t = cloneTemplate(tpl);
  const embed = embedOf(t);
  if (image !== undefined) embed.image = image;
  if (thumbnail !== undefined) embed.thumbnail = thumbnail;
  return t;
}

/**
 * Sets the author line. A clear word as `text` removes it; otherwise the given parts are merged into the current one
 * (the caller converts clear words in `iconUrl`/`url` to null and validates the URLs).
 * @returns {object} a new template
 */
export function setAuthor(tpl, { text, iconUrl, url } = {}) {
  const t = cloneTemplate(tpl);
  const embed = embedOf(t);
  if (text !== undefined && isClearWord(text)) {
    embed.author = null;
    return t;
  }
  const cur = embed.author ?? null;
  if (!cur && text === undefined) throw new EmbedEditError('Set the author text first (text:).');
  const name = text !== undefined ? String(text).trim() : cur.name;
  if (!name) throw new EmbedEditError('The author text cannot be empty.');
  embed.author = {
    name,
    iconUrl: iconUrl === undefined ? cur?.iconUrl ?? null : iconUrl,
    url: url === undefined ? cur?.url ?? null : url,
  };
  return t;
}

/**
 * Sets the footer and/or the timestamp flag. A clear word as `text` removes the footer.
 * @returns {object} a new template
 */
export function setFooter(tpl, { text, iconUrl, timestamp } = {}) {
  const t = cloneTemplate(tpl);
  const embed = embedOf(t);
  if (text !== undefined && isClearWord(text)) {
    embed.footer = null;
  } else if (text !== undefined || iconUrl !== undefined) {
    const cur = embed.footer ?? null;
    if (!cur && text === undefined) throw new EmbedEditError('Set the footer text first (text:).');
    const body = text !== undefined ? String(text).trim() : cur.text;
    if (!body) throw new EmbedEditError('The footer text cannot be empty.');
    embed.footer = { text: body, iconUrl: iconUrl === undefined ? cur?.iconUrl ?? null : iconUrl };
  }
  if (typeof timestamp === 'boolean') embed.timestamp = timestamp;
  return t;
}

/** @param {number|null} color - 0..0xFFFFFF, or null for the server default. @returns {object} a new template */
export function setColor(tpl, color) {
  const t = cloneTemplate(tpl);
  embedOf(t).color = color;
  return t;
}

/** @param {string|null} url - the title link, or null to remove it. @returns {object} a new template */
export function setTitleUrl(tpl, url) {
  const t = cloneTemplate(tpl);
  embedOf(t).url = url;
  return t;
}

// ── Import ─────────────────────────────────────────────────────────────────────

function discordShapeToTemplate(obj, warnings) {
  const embeds = Array.isArray(obj.embeds) ? obj.embeds : [];
  if (embeds.length > 1) warnings.push('Only the first embed was imported.');
  const buttons = [];
  let skippedComponent = false;
  for (const row of Array.isArray(obj.components) ? obj.components : []) {
    for (const c of Array.isArray(row?.components) ? row.components : []) {
      if (c?.type === 2 && c?.style === 5 && c?.url) {
        const emoji = c.emoji
          ? (c.emoji.id ? `<${c.emoji.animated ? 'a' : ''}:${c.emoji.name}:${c.emoji.id}>` : c.emoji.name ?? null)
          : null;
        buttons.push({ label: c.label ?? '', url: c.url, emoji });
      } else {
        skippedComponent = true;
      }
    }
  }
  if (skippedComponent) warnings.push('Only link buttons were imported.');
  return {
    content: typeof obj.content === 'string' ? obj.content : null,
    embed: embeds[0] ? specFromEmbed(embeds[0]) : null,
    buttons,
  };
}

/**
 * Parses imported JSON: our own export, a bare embed, Discord message JSON or a Discohook backup. Pure.
 * @param {string} text
 * @returns {{ template: object|null, errors: string[], warnings: string[] }} `template` is null when `errors` is not empty
 */
export function parseImportPayload(text) {
  const warnings = [];
  const fail = (...errors) => ({ template: null, errors, warnings });
  if (typeof text !== 'string') return fail('Attach a .json file or paste JSON.');
  if (Buffer.byteLength(text, 'utf8') > IMPORT_MAX_BYTES) return fail('The JSON must be 20 KB or smaller.');
  let obj;
  try {
    obj = JSON.parse(text);
  } catch {
    return fail('That is not valid JSON.');
  }
  // A Discohook backup wraps the messages: { messages: [{ data: { content, embeds, components } }] }.
  if (obj && typeof obj === 'object' && Array.isArray(obj.messages)) {
    if (obj.messages.length > 1) warnings.push('Only the first message was imported.');
    obj = obj.messages[0]?.data ?? {};
  }
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) return fail('The JSON must be an object.');

  const isDiscordShape = (Array.isArray(obj.embeds) || Array.isArray(obj.components)) && !Object.hasOwn(obj, 'embed');
  const template = normalizeTemplate(isDiscordShape ? discordShapeToTemplate(obj, warnings) : obj);
  const errors = validateForSave(template);
  return { template: errors.length ? null : template, errors, warnings };
}

// ── Welcome Channel presets ────────────────────────────────────────────────────

/**
 * The four Welcome Channel presets (no {user…} tokens: they render with the admin who runs the command).
 * @param {{ inviteUrl?: string|null, modRoleIds?: string[] }} [opts]
 * @returns {Array<{ name: string, template: object }>} in PRESET_NAMES order
 */
export function buildWelcomePresets({ inviteUrl = null, modRoleIds = [] } = {}) {
  const roles = Array.isArray(modRoleIds) ? modRoleIds : [];
  const raw = [
    { name: 'welcome-greeting', template: { content: null, buttons: [], embed: {
      title: '👋 Welcome to {server}!',
      description: "We're glad you're here.\n\n📜 Read the rules below.\n🔗 Check out the links.\n💬 Then come and say hi!",
      thumbnail: '{server.icon}',
      footer: { text: '{server}', iconUrl: '{server.icon}' },
    } } },
    { name: 'welcome-rules', template: { content: null, embed: {
      title: '📜 Server Rules',
      description: '**1.** Be respectful. No harassment, hate speech or discrimination.\n**2.** No spam or self-promotion without permission.\n**3.** Keep content safe for work and in the right channels.\n**4.** No illegal content and no sharing of personal information.\n**5.** Follow the Discord Terms of Service and Community Guidelines.\n**6.** The moderators have the final say.',
      footer: { text: 'Breaking the rules can lead to a warning, mute, kick or ban.' },
    }, buttons: [
      { label: 'Discord Terms', url: 'https://discord.com/terms', emoji: '📄' },
      { label: 'Community Guidelines', url: 'https://discord.com/guidelines', emoji: '🛡️' },
    ] } },
    { name: 'welcome-links', template: { content: null, embed: {
      title: '🔗 Links',
      description: inviteUrl
        ? 'Invite your friends with the button below!\n\nAdmins: add your website and socials with `/embed button add name:welcome-links`.'
        : 'Admins: add your website, socials and an invite link with `/embed button add name:welcome-links`.',
    }, buttons: inviteUrl ? [{ label: 'Invite your friends', url: inviteUrl, emoji: '📨' }] : [] } },
    { name: 'welcome-team', template: { content: null, buttons: [], embed: {
      title: '🛡️ Staff Team',
      description: '**Owner:** {server.owner}\n'
        + (roles.length ? `**Moderators:** ${joinWithLimit(roles.map((id) => `<@&${id}>`), { sep: ' ', max: 1500 })}\n` : '')
        + '\nNeed help? Reach out to a member of the staff team.',
    } } },
  ];
  return raw.map(({ name, template }) => ({ name, template: normalizeTemplate(template) }));
}

// ── messageDelete hook ─────────────────────────────────────────────────────────

/** Hook filter (sync, cheap): only the bot's own messages, or an uncached one, can be a published embed. */
export function isEmbedDeleteCandidate(ctx) {
  const m = ctx?.message;
  if (!m?.id || !ctx?.guildId) return false;
  if (m.partial) return true; // uncached: the author is unknown, so ask the DB
  return m.author?.id === ctx.client?.user?.id;
}

/** Hook body: a deleted published message makes its saved embed a draft again (channelId is kept as a hint). */
export async function onEmbedMessageDelete(ctx) {
  try {
    await prisma.savedEmbed.updateMany({
      where: { guildId: ctx.guildId, messageId: ctx.message.id },
      data: { messageId: null },
    });
  } catch (err) {
    logger.warn('embeds: could not clear a deleted message id', err);
  }
}

// ── Data access ────────────────────────────────────────────────────────────────

/** @returns {Promise<object|null>} the SavedEmbed row; throws on a database failure */
export async function getSavedEmbed(guildId, name) {
  return prisma.savedEmbed.findUnique({ where: { guildId_name: { guildId, name } } });
}

/**
 * @returns {Promise<object>} the SavedEmbed row
 * @throws {EmbedEditError} code NOTFOUND when there is no such embed; any database failure
 */
export async function requireSavedEmbed(guildId, name) {
  const row = await getSavedEmbed(guildId, name);
  if (!row) throw new EmbedEditError(notFoundText(name), 'NOTFOUND');
  return row;
}

/** @returns {Promise<number>} how many saved embeds the guild has; throws on a database failure */
export async function countSavedEmbeds(guildId) {
  return prisma.savedEmbed.count({ where: { guildId } });
}

/**
 * Every saved embed of a guild (about 500 rows at most), ordered by name, for /embed list.
 * @returns {Promise<Array<{ name: string, channelId: string|null, messageId: string|null, updatedAt: Date }>>}
 */
export async function listSavedEmbeds(guildId) {
  return prisma.savedEmbed.findMany({
    where: { guildId },
    orderBy: { name: 'asc' },
    take: GUILD_LIMITS.SAVED_EMBEDS + 50, // the cap is soft: concurrent creates may overshoot a little
    select: { name: true, channelId: true, messageId: true, updatedAt: true },
  });
}

/**
 * Autocomplete lookup: names containing the (normalised) query, case-insensitive, ordered by name.
 * Never throws.
 * @param {string} guildId
 * @param {string} query
 * @param {number} [take=25]
 * @returns {Promise<Array<{ id: string, name: string, channelId: string|null, messageId: string|null }>>}
 */
export async function searchSavedEmbeds(guildId, query, take = 25) {
  try {
    const q = normalizeEmbedName(String(query ?? '')) ?? '';
    const limit = Math.min(Math.max(Math.floor(Number(take)) || 25, 1), 100);
    return await prisma.savedEmbed.findMany({
      where: { guildId, ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}) },
      orderBy: { name: 'asc' },
      take: limit,
      select: { id: true, name: true, channelId: true, messageId: true },
    });
  } catch (err) {
    logger.debug(`embeds: searchSavedEmbeds failed: ${err?.message}`);
    return [];
  }
}

/**
 * The §10 contract for other packages: the raw template of a saved embed (placeholders unexpanded), or null when
 * it does not exist, the name is invalid, or anything fails. Does NOT check the embeds module toggle (the caller
 * decides). Fail-open cache: any Redis trouble falls through to Postgres; readers fill with SET NX so they can
 * never overwrite a newer write-through value; a stale template lives at most 300 s.
 * @param {string} guildId
 * @param {string} name - normalised here, so "My Rules" and "my-rules" are the same embed
 * @returns {Promise<object|null>}
 */
export async function getSavedEmbedTemplate(guildId, name) {
  try {
    const n = normalizeEmbedName(name);
    if (!guildId || !n) return null;
    const key = embedCacheKey(guildId, n);
    const hit = await cache.get(key);
    if (hit && typeof hit === 'object') return hit.__none ? null : normalizeTemplate(hit);
    const row = await prisma.savedEmbed.findUnique({
      where: { guildId_name: { guildId, name: n } },
      select: { template: true },
    });
    if (!row) {
      await cache.setNX(key, { __none: true }, EMBED_CACHE_MISS_TTL_SECONDS);
      return null;
    }
    const tpl = normalizeTemplate(row.template);
    await cache.setNX(key, tpl, EMBED_CACHE_TTL_SECONDS);
    return tpl;
  } catch (err) {
    logger.warn('embeds: getSavedEmbedTemplate failed', err);
    return null;
  }
}

/**
 * Creates a saved embed. Validates, ensures the GuildSettings row, enforces the per-guild cap (a soft cap: concurrent
 * creates may overshoot by the number of simultaneous requests), then writes through to the cache.
 * @returns {Promise<object>} the row
 * @throws {EmbedEditError} code INVALID (with `.errors`/`.template`), LIMIT or TAKEN
 */
export async function createSavedEmbed(guildId, name, template, createdBy) {
  const next = normalizeTemplate(template);
  const errors = validateForSave(next);
  if (errors.length) throw new EmbedEditError(formatErrors(errors), 'INVALID', { errors, template: next });
  await ensureGuildSettings(guildId);
  if ((await countSavedEmbeds(guildId)) >= GUILD_LIMITS.SAVED_EMBEDS) throw new EmbedEditError(limitText(), 'LIMIT');
  let row;
  try {
    row = await prisma.savedEmbed.create({ data: { guildId, name, template: next, createdBy } });
  } catch (err) {
    if (err?.code === 'P2002') throw new EmbedEditError(takenText(name), 'TAKEN');
    throw err;
  }
  await cache.set(embedCacheKey(guildId, name), next, EMBED_CACHE_TTL_SECONDS);
  return row;
}

// Re-reads the row under the per-embed lock, computes the next template from the fresh one, validates it, saves it
// and writes it through to the cache. `compute(freshTemplate, freshRow)` returns a template or { template, errors }.
function lockedUpdate(guildId, id, compute, name = null) {
  return embedLock.run(id, async () => {
    const fresh = await prisma.savedEmbed.findFirst({ where: { id, guildId } });
    if (!fresh) {
      throw new EmbedEditError(name ? notFoundText(name) : 'That saved embed no longer exists. See `/embed list`.', 'NOTFOUND');
    }
    const out = compute(normalizeTemplate(fresh.template), fresh);
    const next = normalizeTemplate(out && Object.hasOwn(out, 'template') && Object.hasOwn(out, 'errors') ? out.template : out);
    const errors = [...new Set([...(out?.errors ?? []), ...validateForSave(next)])];
    if (errors.length) throw new EmbedEditError(formatErrors(errors), 'INVALID', { errors, template: next });
    let saved;
    try {
      saved = await prisma.savedEmbed.update({ where: { id }, data: { template: next } });
    } catch (err) {
      if (err?.code === 'P2025') throw new EmbedEditError(notFoundText(fresh.name), 'NOTFOUND');
      throw err;
    }
    await cache.set(embedCacheKey(guildId, fresh.name), next, EMBED_CACHE_TTL_SECONDS);
    return saved;
  });
}

/**
 * Applies a pure mutator to a saved embed under the per-embed lock.
 * @param {string} guildId
 * @param {string} name - already normalised
 * @param {(tpl: object) => object} mutator
 * @returns {Promise<object>} the saved row
 * @throws {EmbedEditError} NOTFOUND, or INVALID (the mutator's message or the validation list)
 */
export async function mutateSavedEmbed(guildId, name, mutator) {
  const row = await getSavedEmbed(guildId, name);
  if (!row) throw new EmbedEditError(notFoundText(name), 'NOTFOUND');
  return lockedUpdate(guildId, row.id, (tpl) => mutator(tpl), name);
}

/**
 * Replaces the template of a saved embed (import with overwrite).
 * @throws {EmbedEditError} NOTFOUND or INVALID
 */
export async function replaceTemplate(guildId, id, template) {
  return lockedUpdate(guildId, id, () => template);
}

/** Records where an embed was published. A row deleted meanwhile is not an error. */
export async function setPublished(id, { channelId, messageId }) {
  try {
    await prisma.savedEmbed.update({ where: { id }, data: { channelId, messageId } });
  } catch (err) {
    if (err?.code !== 'P2025') throw err;
  }
}

/**
 * Deletes a saved embed and, on request, the message it published.
 * @param {{ guild: object, row: object, deleteMessage?: boolean, client?: object }} args
 * @returns {Promise<{ deleted: boolean, messageDeleted: boolean }>}
 */
export async function deleteSavedEmbedAndMessage({ guild, row, deleteMessage = false }) {
  return embedLock.run(row.id, async () => {
    try {
      await prisma.savedEmbed.delete({ where: { id: row.id } });
    } catch (err) {
      if (err?.code === 'P2025') return { deleted: false, messageDeleted: false };
      throw err;
    }
    await cache.set(embedCacheKey(row.guildId, row.name), { __none: true }, EMBED_CACHE_MISS_TTL_SECONDS);
    let messageDeleted = false;
    if (deleteMessage && row.messageId && row.channelId) {
      try {
        const channel = guild.channels.cache.get(row.channelId) ?? await guild.channels.fetch(row.channelId);
        if (channel) {
          markBotDeletion(row.messageId, 'embeds');
          await channel.messages.delete(row.messageId);
          messageDeleted = true;
        }
      } catch (err) {
        if (apiCode(err) === RESTJSONErrorCodes.UnknownMessage) messageDeleted = true;
        else logger.warn('embeds: could not delete the published message', err);
      }
    }
    return { deleted: true, messageDeleted };
  });
}

// ── Publish and update ─────────────────────────────────────────────────────────

/**
 * Posts a saved embed in a channel and records it as the tracked message. A previous tracked copy is left alone.
 * @param {{ row: object, channel: object, member: object, settings?: object|null }} args
 * @returns {Promise<{ ok: true, message: object, previous: {channelId: string, messageId: string}|null, refused: string[] }
 *   | { ok: false, code: 'MEMBER_NO_ACCESS'|'CHANNEL'|'EMPTY'|'DISCORD', missing?: string[], reason?: string, error?: Error }>}
 *   `refused` = role ids typed in the text that will not ping (not mentionable and the member may not mention all roles)
 * @throws {EmbedEditError} NOTFOUND when the row vanished; any non-Discord error
 */
export async function publishSavedEmbed({ row, channel, member, settings = null }) {
  return embedLock.run(row.id, async () => {
    // The member needs View Channel and Send Messages there (the owner and Administrators are exempt).
    if (memberChannelProblem(channel, member)) return { ok: false, code: 'MEMBER_NO_ACCESS' };
    const usable = checkChannelUsable(channel);
    if (!usable.ok) return { ok: false, code: 'CHANNEL', missing: usable.missing, reason: usable.reason };

    const fresh = await prisma.savedEmbed.findUnique({ where: { id: row.id } });
    if (!fresh) throw new EmbedEditError(notFoundText(row.name), 'NOTFOUND');
    const payload = renderSavedEmbed(
      normalizeTemplate(fresh.template),
      { member, guild: channel.guild, channel, settings },
      { mentions: 'publish' },
    );
    if (!payload) return { ok: false, code: 'EMPTY' };
    const { refused } = resolvePublishMentions(payload.content, channel, member);

    let message;
    try {
      message = await channel.send(payload);
    } catch (err) {
      if (!(err instanceof DiscordAPIError)) throw err;
      logger.warn(`embeds: publish of "${row.name}" refused by Discord (${err.code})`, err);
      const denied = err.code === RESTJSONErrorCodes.MissingPermissions || err.code === RESTJSONErrorCodes.MissingAccess;
      return { ok: false, code: denied ? 'CHANNEL' : 'DISCORD', error: err };
    }
    const previous = fresh.messageId ? { channelId: fresh.channelId, messageId: fresh.messageId } : null;
    await setPublished(row.id, { channelId: channel.id, messageId: message.id });
    return { ok: true, message, previous, refused };
  });
}

/**
 * Edits the published message of a saved embed to match the current template (re-rendered with `member` as {user};
 * an edit never pings). A deleted message or channel turns the embed back into a draft (REST 10008 / 10003 only).
 * @param {{ row: object, guild: object, member: object, settings?: object|null }} args
 * @returns {Promise<{ ok: true, message: object, channel: object }
 *   | { ok: false, code: 'NOT_PUBLISHED'|'CHANNEL_GONE'|'MESSAGE_GONE'|'NO_ACCESS'|'ARCHIVED'|'EMPTY'|'DISCORD'|'UNAVAILABLE',
 *       missing?: string[], error?: Error, row?: object }>}
 */
export async function updatePublishedEmbed({ row, guild, member, settings = null }) {
  return embedLock.run(row.id, async () => {
    const fresh = await prisma.savedEmbed.findUnique({ where: { id: row.id } });
    if (!fresh) throw new EmbedEditError(notFoundText(row.name), 'NOTFOUND');
    if (!fresh.messageId || !fresh.channelId) return { ok: false, code: 'NOT_PUBLISHED', row: fresh };
    // Rule 20: a missing entity only counts when the guild is available and Discord itself said "unknown".
    if (guild.available === false) return { ok: false, code: 'UNAVAILABLE' };

    const draftAgain = (data) => prisma.savedEmbed.update({ where: { id: fresh.id }, data }).catch((err) => {
      if (err?.code !== 'P2025') throw err;
    });

    let channel = guild.channels.cache.get(fresh.channelId) ?? null;
    if (!channel) {
      try {
        channel = await guild.channels.fetch(fresh.channelId);
      } catch (err) {
        const code = apiCode(err);
        if (code === RESTJSONErrorCodes.UnknownChannel) channel = null;
        else if (code === RESTJSONErrorCodes.MissingAccess || code === RESTJSONErrorCodes.MissingPermissions) {
          return { ok: false, code: 'NO_ACCESS', missing: [] };
        } else if (err instanceof DiscordAPIError) {
          return { ok: false, code: 'DISCORD', error: err };
        } else throw err;
      }
      if (!channel) {
        await draftAgain({ channelId: null, messageId: null });
        return { ok: false, code: 'CHANNEL_GONE' };
      }
    }
    // Editing a message the member cannot see would let them change a private channel's embed.
    if (memberChannelProblem(channel, member, ['ViewChannel'])) return { ok: false, code: 'MEMBER_NO_ACCESS' };
    const usable = checkChannelUsable(channel, { needs: ['ViewChannel', 'ReadMessageHistory', 'EmbedLinks'] });
    if (!usable.ok) return { ok: false, code: 'NO_ACCESS', missing: usable.missing };

    let message;
    try {
      message = await channel.messages.fetch(fresh.messageId);
    } catch (err) {
      const code = apiCode(err);
      if (code === RESTJSONErrorCodes.UnknownMessage) {
        await draftAgain({ messageId: null });
        return { ok: false, code: 'MESSAGE_GONE' };
      }
      if (code === RESTJSONErrorCodes.MissingAccess || code === RESTJSONErrorCodes.MissingPermissions) return { ok: false, code: 'NO_ACCESS', missing: [] };
      if (err instanceof DiscordAPIError) return { ok: false, code: 'DISCORD', error: err };
      throw err;
    }
    if (message.author?.id !== guild.client.user.id) {
      await draftAgain({ messageId: null });
      return { ok: false, code: 'MESSAGE_GONE' };
    }

    const payload = renderSavedEmbed(
      normalizeTemplate(fresh.template),
      { member, guild, channel, settings },
      { mentions: 'none' },
    );
    if (!payload) return { ok: false, code: 'EMPTY' };

    try {
      // An empty content clears old text; [] clears old embeds and buttons.
      await message.edit({
        content: payload.content ?? '',
        embeds: payload.embeds,
        components: payload.components,
        allowedMentions: { parse: [] },
      });
    } catch (err) {
      const code = apiCode(err);
      if (code === RESTJSONErrorCodes.CannotEditMessageAuthoredByAnotherUser || code === RESTJSONErrorCodes.UnknownMessage) {
        await draftAgain({ messageId: null });
        return { ok: false, code: 'MESSAGE_GONE' };
      }
      if (code === RESTJSONErrorCodes.InvalidActionOnArchivedThread || code === RESTJSONErrorCodes.ThreadLocked) return { ok: false, code: 'ARCHIVED' };
      if (code === RESTJSONErrorCodes.MissingPermissions || code === RESTJSONErrorCodes.MissingAccess) return { ok: false, code: 'NO_ACCESS', missing: [] };
      if (err instanceof DiscordAPIError) {
        logger.warn(`embeds: update of "${fresh.name}" refused by Discord (${code})`, err);
        return { ok: false, code: 'DISCORD', error: err };
      }
      throw err;
    }
    return { ok: true, message, channel };
  });
}

// ── Welcome Channel ────────────────────────────────────────────────────────────

async function resolveInvite(guild, channel) {
  if (guild.vanityURLCode) return { source: 'vanity', url: `https://discord.gg/${guild.vanityURLCode}` };
  if (missingBotPermissions(channel, ['CreateInstantInvite']).length === 0) {
    try {
      // unique: false reuses an equivalent invite, so re-running does not pile up permanent invites.
      const invite = await channel.createInvite({ maxAge: 0, maxUses: 0, unique: false, reason: 'Welcome Channel invite link' });
      return { source: 'created', url: invite.url };
    } catch (err) {
      logger.debug(`embeds: could not create the welcome invite: ${err?.message}`);
    }
  }
  return { source: 'none', url: null };
}

async function createWelcomeChannel(guild, interaction) {
  const need = ['ManageChannels', 'ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory'];
  const missing = missingBotPermissions(guild, need);
  if (missing.length) return { ok: false, code: 'BOT_PERMS', missing };
  const me = guild.members.me;
  // Discord only lets a bot deny permission bits it has itself.
  const deny = ['SendMessages', 'SendMessagesInThreads', 'CreatePublicThreads', 'CreatePrivateThreads']
    .filter((p) => me.permissions.has(PermissionFlagsBits[p]));
  try {
    const channel = await guild.channels.create({
      name: 'welcome',
      type: ChannelType.GuildText,
      position: 0,
      topic: clampText(`Welcome to ${guild.name}!`, 1024),
      reason: `Welcome channel set up by ${interaction.user.tag}`,
      permissionOverwrites: [
        { id: guild.id, type: OverwriteType.Role, deny: deny.map((p) => PermissionFlagsBits[p]) },
        {
          id: me.id,
          type: OverwriteType.Member,
          allow: [PermissionFlagsBits.ViewChannel, PermissionFlagsBits.SendMessages, PermissionFlagsBits.EmbedLinks, PermissionFlagsBits.ReadMessageHistory],
        },
      ],
    });
    return { ok: true, channel };
  } catch (err) {
    const code = apiCode(err);
    if (code === RESTJSONErrorCodes.MaximumNumberOfGuildChannelsReached) return { ok: false, code: 'TOO_MANY_CHANNELS' };
    if (code === RESTJSONErrorCodes.MissingPermissions) return { ok: false, code: 'BOT_PERMS', missing: ['ManageChannels'] };
    if (err instanceof DiscordAPIError) return { ok: false, code: 'DISCORD', error: err };
    throw err;
  }
}

/**
 * /embed welcome-channel: creates (or reuses) the four editable preset embeds and posts them in order.
 * Existing presets are never overwritten. Never pings. The command replies (the interaction is already deferred).
 * @param {{ interaction: object, create?: boolean|null, channel?: object|null, settings?: object|null }} args
 * @returns {Promise<object>} `{ ok: true, channel, createdChannel, createdPresets, reusedPresets, tip, invite }` or
 *   `{ ok: false, code }` with code LIMIT | CHANNEL | ALREADY | BOT_PERMS | TOO_MANY_CHANNELS | WRONG_CHANNEL | DISCORD | PARTIAL
 */
export async function setUpWelcomeChannel({ interaction, create = true, channel = null, settings = null }) {
  const guild = interaction.guild;
  const guildId = guild.id;

  // 1. Presets present, and room for the missing ones. Runs before any channel is created.
  const existing = await prisma.savedEmbed.findMany({ where: { guildId, name: { in: [...PRESET_NAMES] } } });
  const byName = new Map(existing.map((r) => [r.name, r]));
  const missingNames = PRESET_NAMES.filter((n) => !byName.has(n));
  const count = await countSavedEmbeds(guildId);
  if (count + missingNames.length > GUILD_LIMITS.SAVED_EMBEDS) {
    return { ok: false, code: 'LIMIT', needed: missingNames.length, count };
  }

  // 2. Target channel.
  let target = channel;
  let createdChannel = false;
  let tip = null;
  if (target) {
    const usable = checkChannelUsable(target);
    if (!usable.ok) return { ok: false, code: 'CHANNEL', channel: target, missing: usable.missing, reason: usable.reason };
    if (target.permissionsFor(guild.roles.everyone)?.has(PermissionFlagsBits.SendMessages)) {
      tip = 'Tip: members can write in this channel; make it read-only if you want a MEE6-style welcome channel.';
    }
  } else if (create !== false) {
    const greeting = byName.get('welcome-greeting');
    if (greeting?.messageId && greeting.channelId && guild.channels.cache.has(greeting.channelId)) {
      return { ok: false, code: 'ALREADY', channelId: greeting.channelId };
    }
    const made = await createWelcomeChannel(guild, interaction);
    if (!made.ok) return made;
    target = made.channel;
    createdChannel = true;
  } else {
    target = interaction.channel;
    if (!target || ![ChannelType.GuildText, ChannelType.GuildAnnouncement].includes(target.type)) {
      return { ok: false, code: 'WRONG_CHANNEL' };
    }
    const usable = checkChannelUsable(target);
    if (!usable.ok) return { ok: false, code: 'CHANNEL', channel: target, missing: usable.missing, reason: usable.reason };
  }

  // 3. Invite link, only when the links preset has to be created.
  const invite = missingNames.includes('welcome-links') ? await resolveInvite(guild, target) : null;

  // 4. Create the missing presets. Existing ones (possibly edited by admins) are never overwritten.
  const modRoleIds = (settings?.modRoleIds ?? []).filter((id) => guild.roles.cache.has(id));
  const templates = new Map(buildWelcomePresets({ inviteUrl: invite?.url ?? null, modRoleIds }).map((p) => [p.name, p.template]));
  const createdPresets = [];
  for (const name of missingNames) {
    try {
      byName.set(name, await createSavedEmbed(guildId, name, templates.get(name), interaction.user.id));
      createdPresets.push(name);
    } catch (err) {
      if (err instanceof EmbedEditError && err.code === 'TAKEN') {
        const row = await getSavedEmbed(guildId, name); // a concurrent run created it first
        if (row) { byName.set(name, row); continue; }
      }
      throw err;
    }
  }
  const reusedPresets = PRESET_NAMES.filter((n) => !createdPresets.includes(n));

  // 5. Publish in order; stop at the first failure and report what was posted.
  const posted = [];
  for (const name of PRESET_NAMES) {
    const row = byName.get(name);
    try {
      const outcome = await embedLock.run(row.id, async () => {
        const fresh = await prisma.savedEmbed.findUnique({ where: { id: row.id } });
        if (!fresh) return 'gone';
        const payload = renderSavedEmbed(
          normalizeTemplate(fresh.template),
          { member: interaction.member, guild, channel: target, settings },
          { mentions: 'none' },
        );
        if (!payload) return 'empty';
        const message = await target.send(payload);
        await setPublished(row.id, { channelId: target.id, messageId: message.id });
        return 'posted';
      });
      if (outcome === 'posted') posted.push(name);
    } catch (err) {
      logger.warn(`embeds: welcome-channel stopped at "${name}"`, err);
      return {
        ok: false,
        code: 'PARTIAL',
        channel: target,
        posted,
        unposted: PRESET_NAMES.filter((n) => !posted.includes(n)),
        error: err,
      };
    }
  }
  return { ok: true, channel: target, createdChannel, createdPresets, reusedPresets, posted, tip, invite };
}

// ── Modal submit ───────────────────────────────────────────────────────────────

async function modalSettings(interaction) {
  // The component wrapper already filled the settings memo; a cold load must leave time to defer the reply.
  return peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'embed modal settings').catch(() => null);
}

async function replyQuietly(interaction, payload) {
  try {
    if (interaction.deferred || interaction.replied) await interaction.editReply(payload);
    else await interaction.reply({ ...payload, ephemeral: true });
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('embeds: could not answer the form', err);
  }
}

const draftHint = 'Your text is attached; fix it and use `/embed import`.';

/**
 * Handles the submit of `embed-modal:new:{name}` (create) and `embed-modal:{savedEmbedId}` (edit).
 * Registered as a persistent modal handler by the plugin (already gated by the embeds module).
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 */
export async function handleEmbedModalSubmit(interaction) {
  if (!interaction.inCachedGuild()) {
    await interaction.reply({ embeds: [errorEmbed('Server Only', 'This form only works in a server.')], ephemeral: true }).catch(() => {});
    return;
  }
  const settings = await modalSettings(interaction);
  // Defence in depth: modal submits bypass the router's permission level check.
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) {
    await interaction.reply({
      embeds: [errorEmbed('Insufficient Permissions', 'You need the Admin level to edit saved embeds.')],
      ephemeral: true,
    }).catch(() => {});
    return;
  }
  try {
    await interaction.deferReply();
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn('embeds: could not defer the form reply', err);
    return;
  }

  const guildId = interaction.guildId;
  const renderCtx = { member: interaction.member, user: interaction.user, guild: interaction.guild, channel: interaction.channel };
  try {
    const { params } = parseCustomId(interaction.customId);
    if (params[0] === 'new') {
      const name = normalizeEmbedName(params[1] ?? '');
      if (!name) {
        await replyQuietly(interaction, { embeds: [errorEmbed('Invalid Name', 'Embed names can use letters, numbers, - and _ (up to 32 characters).')] });
        return;
      }
      const { template, errors } = parseTemplateModal(interaction, emptyTemplate());
      const all = [...new Set([...errors, ...validateForSave(template)])];
      if (all.length) {
        await replyQuietly(interaction, {
          embeds: [errorEmbed('Invalid Embed', `${formatErrors(all)}\n\n${draftHint}`)],
          files: [draftAttachment(template)],
        });
        return;
      }
      let row;
      try {
        row = await createSavedEmbed(guildId, name, template, interaction.user.id);
      } catch (err) {
        if (!(err instanceof EmbedEditError)) throw err;
        const title = err.code === 'TAKEN' ? 'Name Taken' : err.code === 'LIMIT' ? 'Limit Reached' : 'Could Not Save';
        await replyQuietly(interaction, { embeds: [errorEmbed(title, `${err.message}\n\n${draftHint}`)], files: [draftAttachment(template)] });
        return;
      }
      const tpl = normalizeTemplate(row.template);
      const embed = successEmbed(
        'Embed Created',
        `Saved **${name}**. Next: \`/embed field add\`, \`/embed button add\`, \`/embed preview\`, then \`/embed publish\`.${unknownVariablesNote(tpl, renderCtx)}`,
      ).addFields({ name: 'Variables', value: clampText(placeholderHelp(['server', 'channel']), 1024) });
      await replyQuietly(interaction, { embeds: [embed] });
      return;
    }

    // Edit: params[0] is the saved embed id.
    const row = await prisma.savedEmbed.findFirst({ where: { id: String(params[0] ?? ''), guildId } });
    if (!row) {
      const { template } = parseTemplateModal(interaction, emptyTemplate());
      await replyQuietly(interaction, {
        embeds: [errorEmbed('Embed Not Found', `This embed was deleted while the form was open.\n\n${draftHint}`)],
        files: [draftAttachment(template)],
      });
      return;
    }
    let saved;
    try {
      saved = await lockedUpdate(guildId, row.id, (tpl) => parseTemplateModal(interaction, tpl), row.name);
    } catch (err) {
      if (!(err instanceof EmbedEditError)) throw err;
      const payload = err.code === 'INVALID'
        ? { embeds: [errorEmbed('Invalid Embed', `${err.message}\n\n${draftHint}`)], files: [draftAttachment(err.template ?? emptyTemplate())] }
        : { embeds: [errorEmbed('Could Not Save', err.message)] };
      await replyQuietly(interaction, payload);
      return;
    }
    const tpl = normalizeTemplate(saved.template);
    await replyQuietly(interaction, {
      embeds: [successEmbed('Embed Saved', `Saved **${row.name}**.${publishHint(saved, row.name)}${unknownVariablesNote(tpl, renderCtx)}`)],
    });
  } catch (err) {
    logger.error('embeds: form submit failed', err);
    await replyQuietly(interaction, {
      embeds: [errorEmbed('Something Went Wrong', 'I could not save that right now. Please try again in a moment.')],
    });
  }
}
