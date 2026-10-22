// ── Welcome & Goodbye ──────────────────────────────────────────────────────────
// Everything behind /welcome (goodbye group included) and the welcome plugin: the default texts, healing of stored
// JSON, the text/embed truth table, payload building (one render path for real joins and previews),
// the card orchestration, the channel / DM / goodbye senders, autoroles (immediate, delayed through the
// durable scheduler, screening watch, captcha gate), the hook and job bodies, and the setup modals.
//
// Never imported from here: the database client, invite tracking, or any other feature package.
// Invite attribution belongs to the invites package (ctx.state.invite carries it).
import { ActionRowBuilder, AttachmentBuilder, ButtonBuilder, ButtonStyle } from 'discord.js';
import { Prisma } from '@prisma/client';
import { getSettings, getSettingsSafe, peekSettings, updateSettings, updateSettingsWith } from './settingsService.js';
import { scheduleJob, cancelJobs, getJob, PermanentJobError } from './scheduler.js';
import { cache } from './redis.js';
import { COLORS, LIMITS, PERMISSION_LEVELS } from '../config/constants.js';
import { buildCustomId } from '../config/customIds.js';
import {
  normalizeLegacyEmbed, renderTemplate, validateTemplate, buildTemplateModal, parseTemplateModal,
  parseColor, formatColor, isHttpUrl, TEMPLATE_FIELD_IDS,
} from '../utils/customEmbed.js';
import { renderPlaceholders, renderPlaceholdersDetailed } from '../utils/placeholderParser.js';
import {
  checkChannelUsable, checkAssignableRole, missingBotPermissions, permissionNames,
} from '../utils/interactionHelpers.js';
import { clampText, joinWithLimit } from '../utils/text.js';
import { parseDuration, formatDurationLong } from '../utils/parseDuration.js';
import { ackBudgetMs, withTimeout, createThrottle } from '../utils/async.js';
import { successEmbed, errorEmbed, warnEmbed, infoEmbed, applyGuildColor } from '../utils/embedBuilder.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import { generateWelcomeCard, validateCardBackground } from '../utils/welcomeCard.js';
import logger from '../utils/logger.js';

// ── Constants ──────────────────────────────────────────────────────────────────

export const DEFAULT_WELCOME_MESSAGE = 'Hey {user}, welcome to **{server}**!';
export const DEFAULT_WELCOME_DM_MESSAGE = 'Have a great time here in **{server}**!';
export const DEFAULT_GOODBYE_MESSAGE = '**{user.idname}** just left the server';

// The embed does not repeat the greeting: the content line carries it.
export const DEFAULT_WELCOME_EMBED = Object.freeze({
  title: '👋 Welcome to {server}!',
  description: 'You are our **{server.member_count.ordinal}** member. Enjoy your stay!',
  thumbnail: '{user.avatar}',
  color: 0x57F287,
  timestamp: true,
});
export const DEFAULT_GOODBYE_EMBED = Object.freeze({
  description: DEFAULT_GOODBYE_MESSAGE,
  thumbnail: '{user.avatar}',
  color: 0x99AAB5,
  timestamp: true,
});

export const MAX_AUTOROLE_DELAY_SECONDS = 604_800; // 7 days
export const SCREENING_WATCH_MS = 7 * 24 * 3_600_000; // give up on members still pending after this
export const SCREENING_POLL_MS = 6 * 3_600_000; // watch-job poll interval
export const CARD_TIMEOUT_MS = 6_000; // overall card budget inside the join hook
export const FLOOD_CAPS = Object.freeze({ msg: 60, dm: 30, bye: 60, card: 60 }); // per guild per minute
export const CARD_FILE_NAME = 'welcome.png';

export class WelcomeConfigError extends Error {
  constructor(message, title) {
    super(message);
    this.name = 'WelcomeConfigError';
    this.title = title;
  }
}

export const autoroleJobKey = (guildId, userId) => `welcome.autorole:${guildId}:${userId}`;

const MODAL_IDS = Object.freeze({
  welcome: buildCustomId('welcome-setup'),
  dm: buildCustomId('welcome-dm-setup'),
  goodbye: buildCustomId('goodbye-setup'),
});

const DEFAULT_MESSAGES = Object.freeze({
  welcome: DEFAULT_WELCOME_MESSAGE,
  dm: DEFAULT_WELCOME_DM_MESSAGE,
  goodbye: DEFAULT_GOODBYE_MESSAGE,
});

export const MESSAGE_FIELDS = Object.freeze({ welcome: 'welcomeMessage', dm: 'welcomeDmMessage', goodbye: 'goodbyeMessage' });
const EMBED_FIELDS = Object.freeze({ welcome: 'welcomeEmbed', dm: 'welcomeDmEmbed', goodbye: 'goodbyeEmbed' });
const JSON_COLUMNS = Object.freeze(['welcomeEmbed', 'welcomeCardStyle', 'welcomeDmEmbed', 'goodbyeEmbed']);

const warnThrottle = createThrottle(10 * 60_000); // operational warnings, per `${guildId}:${topic}`
const floodThrottle = createThrottle(60_000); // "flood cap reached", once per guild, kind and minute

const warnOnce = (key, message) => { if (warnThrottle(key)) logger.warn(message); };
const memoOnce = (fn) => { let p; return () => (p ??= Promise.resolve().then(fn)); };
const isPlainObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const clampInt = (v, lo, hi) => {
  const n = Math.trunc(Number(v));
  return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : lo;
};
const validColor = (c) => Number.isInteger(c) && c >= 0 && c <= 0xffffff;
const nonEmpty = (v) => (typeof v === 'string' && v !== '' ? v : null);
const channelLabel = (id) => (id ? `<#${id}>` : 'not set');

// ── Writing settings ───────────────────────────────────────────────────────────
// A plain `null` written to a Json column is stored as the JSON literal null, not SQL NULL; Prisma.DbNull is
// how a Json column is really cleared.

/**
 * Converts `null` for the Json columns this package writes into Prisma.DbNull. Pure; returns a copy.
 * @param {object|null|undefined} data
 * @returns {object|null|undefined}
 */
export function toDbData(data) {
  if (!data) return data;
  const out = { ...data };
  for (const key of JSON_COLUMNS) {
    if (key in out && out[key] === null) out[key] = Prisma.DbNull;
  }
  return out;
}

/** updateSettings with null Json values mapped to DbNull. */
export function saveSettings(guildId, data) {
  return updateSettings(guildId, toDbData(data));
}

/** updateSettingsWith whose mutator result gets the same DbNull mapping. */
export function saveSettingsWith(guildId, mutator) {
  return updateSettingsWith(guildId, async (cur) => toDbData(await mutator(cur)));
}

// ── Embed JSON: healing, content test, serialisation ───────────────────────────

const isPlaceholderUrl = (s) => typeof s === 'string' && s.includes('{') && s.includes('}');
const imageUrlOk = (s) => typeof s === 'string' && (isHttpUrl(s) || isPlaceholderUrl(s) || /^attachment:\/\/[^\s/]{1,200}$/.test(s));

/**
 * True when the spec would show something: a non-empty title, description, field list, author name
 * or image. A footer-only or thumbnail-only embed counts as empty (R053).
 * @param {object|null|undefined} spec
 * @returns {boolean}
 */
export function hasEmbedContent(spec) {
  if (!isPlainObject(spec)) return false;
  const nonBlank = (v) => typeof v === 'string' && v.trim() !== '';
  return nonBlank(spec.title)
    || nonBlank(spec.description)
    || (Array.isArray(spec.fields) && spec.fields.length > 0)
    || nonBlank(spec.author?.name)
    || nonBlank(spec.image);
}

/**
 * Turns a stored embed (new EmbedSpec or the legacy shape) into a safe EmbedSpec, or null when it has
 * no content. Never throws: invalid colours are dropped, strings pre-clamped, bad image URLs removed.
 * @param {unknown} json
 * @returns {object|null}
 */
export function healEmbed(json) {
  try {
    if (!isPlainObject(json)) return null;
    let spec;
    try {
      spec = normalizeLegacyEmbed(json);
    } catch {
      return null;
    }
    if (!validColor(spec.color)) delete spec.color;
    if (spec.title) spec.title = clampText(spec.title, 256);
    if (spec.description) spec.description = clampText(spec.description, 4096);
    if (spec.footer) {
      if (spec.footer.text) spec.footer = { ...spec.footer, text: clampText(spec.footer.text, 2048) };
      else delete spec.footer;
    }
    if (spec.author) {
      if (spec.author.name) spec.author = { ...spec.author, name: clampText(spec.author.name, 256) };
      else delete spec.author;
    }
    if (Array.isArray(spec.fields)) {
      spec.fields = spec.fields.slice(0, 25).map((f) => ({
        name: clampText(f.name, 256), value: clampText(f.value, 1024), inline: Boolean(f.inline),
      }));
      if (!spec.fields.length) delete spec.fields;
    }
    for (const key of ['thumbnail', 'image']) {
      if (spec[key] !== undefined && !imageUrlOk(spec[key])) delete spec[key];
    }
    if (spec.author?.iconUrl && !imageUrlOk(spec.author.iconUrl)) spec.author = { ...spec.author, iconUrl: null };
    if (spec.footer?.iconUrl && !imageUrlOk(spec.footer.iconUrl)) spec.footer = { ...spec.footer, iconUrl: null };
    return hasEmbedContent(spec) ? spec : null;
  } catch {
    return null;
  }
}

const EMBED_KEYS = ['title', 'description', 'url', 'color', 'author', 'footer', 'thumbnail', 'image', 'fields', 'timestamp'];
const present = (v) => v !== null && v !== undefined && v !== '';

function copyPresent(obj) {
  const out = {};
  for (const [k, v] of Object.entries(obj ?? {})) if (present(v)) out[k] = v;
  return out;
}

/**
 * The shape that is written to the database: only the EmbedSpec keys that carry a value (nested objects
 * the same way). The legacy `card` key is never written.
 * @param {object} spec
 * @returns {object}
 */
export function serializeEmbed(spec) {
  const out = {};
  if (!isPlainObject(spec)) return out;
  for (const key of EMBED_KEYS) {
    const v = spec[key];
    if (!present(v)) continue;
    if (key === 'author' || key === 'footer') {
      const nested = copyPresent(v);
      if (Object.keys(nested).length) out[key] = nested;
    } else if (key === 'fields') {
      if (Array.isArray(v) && v.length) out.fields = v.map((f) => copyPresent(f));
    } else {
      out[key] = v;
    }
  }
  return out;
}

// ── Card style JSON ────────────────────────────────────────────────────────────

const HEX_RE = /^#[0-9a-f]{6}$/i;

/**
 * @param {unknown} json
 * @returns {{ enabled: boolean|null, background: string|null, backgroundUrl: string|null,
 *   textColor: string|null, accentColor: string|null, overlayOpacity: number|null }}
 */
export function parseCardStyle(json) {
  const out = { enabled: null, background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null };
  try {
    if (!isPlainObject(json)) return out;
    const color = (v) => (typeof v === 'string' && HEX_RE.test(v) ? v.toLowerCase() : null);
    out.background = color(json.background);
    out.textColor = color(json.textColor);
    out.accentColor = color(json.accentColor);
    out.backgroundUrl = typeof json.backgroundUrl === 'string' && json.backgroundUrl.length <= 2048 && isHttpUrl(json.backgroundUrl)
      ? json.backgroundUrl : null;
    if (typeof json.overlayOpacity === 'number' && Number.isFinite(json.overlayOpacity)) {
      out.overlayOpacity = Math.round(Math.min(1, Math.max(0, json.overlayOpacity)) * 100) / 100;
    }
    if (typeof json.enabled === 'boolean') out.enabled = json.enabled;
  } catch {
    // fall through with the all-null result
  }
  return out;
}

/**
 * The v1 shape that is written to GuildSettings.welcomeCardStyle (every key present).
 * @param {{ enabled?: boolean|null, background?: string|null, backgroundUrl?: string|null, textColor?: string|null,
 *   accentColor?: string|null, overlayOpacity?: number|null }} style
 */
export function serializeCardStyle(style) {
  const s = style ?? {};
  return {
    v: 1,
    enabled: typeof s.enabled === 'boolean' ? s.enabled : true, // the legacy default was "card on"
    background: s.background ?? null,
    backgroundUrl: s.backgroundUrl ?? null,
    textColor: s.textColor ?? null,
    accentColor: s.accentColor ?? null,
    overlayOpacity: s.overlayOpacity ?? null,
  };
}

/**
 * Whether the welcome card is on. Precedence: the style's own flag, then a legacy `welcomeEmbed.card`
 * of false, otherwise on.
 * @param {object|null} settings
 * @returns {boolean}
 */
export function welcomeCardEnabled(settings) {
  const { enabled } = parseCardStyle(settings?.welcomeCardStyle);
  if (typeof enabled === 'boolean') return enabled;
  const legacy = settings?.welcomeEmbed;
  if (isPlainObject(legacy)) return legacy.card !== false;
  return true;
}

/**
 * Must wrap every mutator result that writes `welcomeEmbed`. It preserves a legacy `card: false`
 * (the legacy key disappears with the rewrite) by moving the flag into `welcomeCardStyle`, and freezes
 * the channel text that was effective before the embed changed, so editing an image never makes the
 * ping text appear or vanish. Mutates and returns `data`.
 * @param {object} cur the current settings row
 * @param {object} data the fields about to be written
 * @returns {object}
 */
export function materialiseWelcome(cur, data) {
  if (typeof parseCardStyle(cur?.welcomeCardStyle).enabled !== 'boolean' && !('welcomeCardStyle' in data)) {
    data.welcomeCardStyle = serializeCardStyle({
      ...parseCardStyle(cur?.welcomeCardStyle),
      enabled: welcomeCardEnabled(cur),
    });
  }
  if ((cur?.welcomeMessage === null || cur?.welcomeMessage === undefined) && !('welcomeMessage' in data)) {
    data.welcomeMessage = resolveTemplate('welcome', cur).content ?? '';
  }
  return data;
}

// ── Template resolution (the truth table) ──────────────────────────────────────

/**
 * What a message consists of, before rendering. `''` content becomes null. Never throws.
 * `forceText` (no EmbedLinks) drops every embed.
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {object|null} settings
 * @param {{ forceText?: boolean }} [opts]
 * @returns {{ content: string|null, embed: object|null }}
 */
export function resolveTemplate(kind, settings, { forceText = false } = {}) {
  try {
    const s = settings ?? {};
    if (kind === 'dm') {
      return {
        content: nonEmpty(s.welcomeDmMessage ?? DEFAULT_WELCOME_DM_MESSAGE),
        embed: forceText ? null : healEmbed(s.welcomeDmEmbed),
      };
    }
    if (kind === 'goodbye') {
      const m = s.goodbyeMessage ?? null;
      if (!(s.goodbyeUseEmbed === true && !forceText)) {
        return { content: nonEmpty(m ?? DEFAULT_GOODBYE_MESSAGE), embed: null };
      }
      const custom = healEmbed(s.goodbyeEmbed);
      return { content: nonEmpty(m), embed: custom ?? DEFAULT_GOODBYE_EMBED };
    }
    const m = s.welcomeMessage ?? null;
    if (!(s.welcomeUseEmbed !== false && !forceText)) {
      return { content: nonEmpty(m ?? DEFAULT_WELCOME_MESSAGE), embed: null };
    }
    const custom = healEmbed(s.welcomeEmbed);
    if (custom) return { content: nonEmpty(m), embed: custom }; // legacy configs keep sending no text line
    return { content: nonEmpty(m ?? DEFAULT_WELCOME_MESSAGE), embed: DEFAULT_WELCOME_EMBED };
  } catch {
    return { content: null, embed: null };
  }
}

// ── Payload building ───────────────────────────────────────────────────────────

function dmButtonRow(guild) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder()
      .setCustomId(buildCustomId('noop', 'welcome-dm'))
      .setLabel(clampText(`Sent from ${guild?.name ?? 'a server'}`, 80))
      .setStyle(ButtonStyle.Secondary)
      .setDisabled(true),
  );
}

/**
 * Builds the message for a join, DM or leave. One render path for real events and for `/… test`.
 * Never throws: on any error it returns the default text (or null when even that fails).
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {{ settings: object, member?: object|null, user?: object|null, guild: object, channel?: object|null,
 *   card?: Buffer|null, forceText?: boolean, preview?: boolean }} opts
 * @returns {object|null} a message payload, or null when nothing would be sent
 */
export function buildWelcomePayload(kind, {
  settings, member = null, user = null, guild, channel = null, card = null, forceText = false, preview = false,
} = {}) {
  const person = member?.user ?? user ?? null;
  const renderCtx = { member, user: person, guild, channel };
  const userId = person?.id ?? member?.id;
  let allowedMentions;
  if (preview) allowedMentions = { parse: [] };
  else if (kind === 'welcome') allowedMentions = { parse: [], users: userId ? [userId] : [] };
  else allowedMentions = { parse: [] };

  let extraComponents = [];
  try {
    extraComponents = kind === 'dm' ? [dmButtonRow(guild)] : [];
    const { content, embed: resolved } = resolveTemplate(kind, settings, { forceText });
    let embed = resolved;

    const files = [];
    if (card) {
      files.push(new AttachmentBuilder(card, { name: CARD_FILE_NAME }));
      if (embed && !embed.image) embed = { ...embed, image: `attachment://${CARD_FILE_NAME}` };
    }

    const configured = settings?.embedColor;
    const defaultColor = validColor(configured) ? configured : (kind === 'goodbye' ? COLORS.NEUTRAL : COLORS.SUCCESS);
    let payload = renderTemplate({ content, embed, buttons: [] }, renderCtx, { defaultColor, allowedMentions, extraComponents });
    if (payload == null && files.length) payload = { embeds: [], components: extraComponents, allowedMentions };
    if (!payload) return null;
    if (files.length) payload.files = files;
    if (kind === 'dm' && !payload.content && !(payload.embeds?.length) && !files.length) return null; // a button alone is empty
    return payload;
  } catch (err) {
    warnOnce(`${guild?.id}:render`, `Welcome ${kind} message could not be rendered in ${guild?.id}: ${err?.message ?? err}`);
    try {
      const content = renderPlaceholders(DEFAULT_MESSAGES[kind], renderCtx, { maxLength: 2000 });
      if (!content) return null;
      return { content, allowedMentions, ...(extraComponents.length ? { components: extraComponents } : {}) };
    } catch {
      return null;
    }
  }
}

// ── Flood caps ─────────────────────────────────────────────────────────────────

async function underFloodCap(guildId, kind) {
  const n = await cache.slidingWindow(`welcome:rate:${guildId}:${kind}`, 60_000);
  // Fail-open: a Redis outage (n === null) must not stop a welcome, DM, goodbye or card.
  if (n !== null && n > FLOOD_CAPS[kind]) {
    if (floodThrottle(`${guildId}:${kind}`)) logger.warn(`Welcome ${kind} flood cap reached in ${guildId}; skipping`);
    return false;
  }
  return true;
}

// ── Card orchestration ─────────────────────────────────────────────────────────

/**
 * Renders the welcome card. Never throws; null means "send without a card".
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings
 * @param {{ preview?: boolean }} [opts] preview bypasses the flood cap
 * @returns {Promise<Buffer|null>}
 */
export async function renderWelcomeCard(member, settings, { preview = false } = {}) {
  const guildId = member?.guild?.id;
  try {
    if (!preview && !(await underFloodCap(guildId, 'card'))) return null; // over the cap: sent without a card
    return await withTimeout(
      generateWelcomeCard(member, member.guild, parseCardStyle(settings?.welcomeCardStyle)),
      CARD_TIMEOUT_MS,
      'welcome card',
    );
  } catch (err) {
    warnOnce(`${guildId}:card`, `Welcome card could not be rendered in ${guildId}: ${err?.message ?? err}`);
    return null;
  }
}

// ── Senders ────────────────────────────────────────────────────────────────────

async function resolveChannel(guild, id) {
  if (!id) return null;
  return guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null);
}

function reportSendError(err, guildId, what) {
  const code = err?.code;
  if (code === 10003 || code === 10004 || code === 50001 || code === 50013) {
    warnOnce(`${guildId}:${what}:${code}`, `${what} could not be sent in ${guildId}: ${err?.message} (${code})`);
    return;
  }
  logger.error(`${what} failed in ${guildId}`, err);
}

// Sends with the documented fallbacks: 50013 with files -> once without the card; 50035 -> once with the default text.
async function sendWithFallback(channel, payload, { guildId, what, withoutCard = null, plain = null }) {
  try {
    await channel.send(payload);
    return;
  } catch (err) {
    let current = err;
    if (current?.code === 50013 && payload.files?.length && withoutCard) {
      const retry = withoutCard();
      if (retry) {
        try {
          await channel.send(retry);
          return;
        } catch (err2) {
          current = err2;
        }
      }
    }
    if (current?.code === 50035 && plain) {
      warnOnce(`${guildId}:${what}:50035`, `${what} had an invalid form body in ${guildId} (${current.message}); sent the default text`);
      const retry = plain();
      if (retry) {
        try {
          await channel.send(retry);
          return;
        } catch (err3) {
          current = err3;
        }
      }
    }
    reportSendError(current, guildId, what);
  }
}

// The channel checks shared by the welcome and goodbye senders. Returns the usable channel or null.
async function usableChannel(guild, id, what) {
  const channel = await resolveChannel(guild, id);
  if (!channel) {
    warnOnce(`${guild.id}:${what}:missing`, `${what} channel ${id} is missing in ${guild.id}`);
    return null;
  }
  const u = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  if (!u.ok) {
    warnOnce(`${guild.id}:${what}:perms`, `${what} channel ${id} is unusable in ${guild.id}: ${u.missing.length ? u.missing.join(', ') : u.reason}`);
    return null;
  }
  return channel;
}

/**
 * Posts the welcome message in the welcome channel. Never throws.
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings
 * @param {() => Promise<Buffer|null>} getCard memoised card renderer (shared with the DM)
 */
export async function sendWelcomeChannel(member, settings, getCard) {
  try {
    if (!(settings?.welcomeEnabled && settings.welcomeMessageEnabled && settings.welcomeChannelId)) return;
    const { guild } = member;
    const channel = await usableChannel(guild, settings.welcomeChannelId, 'Welcome');
    if (!channel) return;
    if (!(await underFloodCap(guild.id, 'msg'))) return;

    const canEmbed = missingBotPermissions(channel, ['EmbedLinks']).length === 0;
    const canAttach = missingBotPermissions(channel, ['AttachFiles']).length === 0;
    // Without AttachFiles the card is dropped instead of losing the whole welcome (R229).
    const card = welcomeCardEnabled(settings) && canAttach ? await getCard() : null;
    const build = (c) => buildWelcomePayload('welcome', { settings, member, guild, channel, card: c, forceText: !canEmbed });
    const payload = build(card);
    if (!payload) return;
    await sendWithFallback(channel, payload, {
      guildId: guild.id,
      what: 'Welcome message',
      withoutCard: () => build(null),
      plain: () => ({
        content: renderPlaceholders(DEFAULT_WELCOME_MESSAGE, { member, user: member.user, guild, channel }, { maxLength: 2000 }),
        allowedMentions: { parse: [], users: [member.id] },
      }),
    });
  } catch (err) {
    logger.error(`Welcome message failed in ${member?.guild?.id}`, err);
  }
}

/**
 * Sends the welcome DM. Bots never get one; closed DMs are skipped silently. Never throws.
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings
 * @param {() => Promise<Buffer|null>} getCard
 */
export async function sendWelcomeDm(member, settings, getCard) {
  try {
    if (!(settings?.welcomeEnabled && settings.welcomeDmEnabled) || member.user?.bot) return;
    const { guild } = member;
    if (!(await underFloodCap(guild.id, 'dm'))) return;
    const card = settings.welcomeDmCard ? await getCard() : null;
    const payload = buildWelcomePayload('dm', { settings, member, guild, channel: null, card });
    if (!payload) return;
    try {
      await member.send(payload);
    } catch (err) {
      if (err?.code === 50007) {
        logger.debug(`Welcome DM skipped for ${member.id}: DMs are closed`);
      } else if (err?.code === 50035) {
        const plain = buildWelcomePayload('dm', {
          settings: { ...settings, welcomeDmMessage: null, welcomeDmEmbed: null }, member, guild, card: null,
        });
        if (plain) await member.send(plain).catch((e) => warnOnce(`${guild.id}:dm`, `Welcome DM failed in ${guild.id}: ${e?.message}`));
      } else {
        warnOnce(`${guild.id}:dm`, `Welcome DM failed in ${guild.id}: ${err?.message} (${err?.code ?? 'error'})`);
      }
    }
  } catch (err) {
    logger.error(`Welcome DM failed in ${member?.guild?.id}`, err);
  }
}

/**
 * Posts the goodbye message for a member who left (kicks and bans included). Never throws.
 * @param {import('discord.js').GuildMember|import('discord.js').PartialGuildMember|null} member may be partial or null
 * @param {import('discord.js').User|null} user
 * @param {object} settings
 * @param {import('discord.js').Guild} [guild] defaults to member.guild
 */
export async function sendGoodbye(member, user, settings, guild = member?.guild) {
  try {
    if (!(settings?.welcomeEnabled && settings.goodbyeEnabled && settings.goodbyeChannelId) || !guild) return;
    const channel = await usableChannel(guild, settings.goodbyeChannelId, 'Goodbye');
    if (!channel) return;
    if (!(await underFloodCap(guild.id, 'bye'))) return;

    const canEmbed = missingBotPermissions(channel, ['EmbedLinks']).length === 0;
    const full = member && !member.partial ? member : null;
    const payload = buildWelcomePayload('goodbye', {
      settings, member: full, user: member?.user ?? user, guild, channel, card: null, forceText: !canEmbed,
    });
    if (!payload) return;
    await sendWithFallback(channel, payload, {
      guildId: guild.id,
      what: 'Goodbye message',
      plain: () => ({
        content: renderPlaceholders(DEFAULT_GOODBYE_MESSAGE, { member: full, user: member?.user ?? user, guild, channel }, { maxLength: 2000 }),
        allowedMentions: { parse: [] },
      }),
    });
  } catch (err) {
    logger.error(`Goodbye message failed in ${guild?.id}`, err);
  }
}

// ── Autoroles: pure helpers ────────────────────────────────────────────────────

const DELAY_ZERO_WORDS = new Set(['0', 'none', 'off', 'no', 'immediate', 'immediately', 'instant']);

/**
 * Parses the autorole delay: '0' / 'none' = immediately, a bare number = seconds, otherwise a duration.
 * @param {unknown} input
 * @returns {{ seconds: number } | { error: string }}
 */
export function parseDelayInput(input) {
  const s = String(input ?? '').trim().toLowerCase();
  if (DELAY_ZERO_WORDS.has(s)) return { seconds: 0 };
  let seconds;
  if (/^\d{1,7}$/.test(s)) {
    seconds = Number(s);
  } else {
    const ms = parseDuration(s);
    if (ms === null) return { error: 'Use a duration like 30s, 10m, 2h or 1d (0 = immediately).' };
    seconds = Math.round(ms / 1000);
  }
  if (seconds > MAX_AUTOROLE_DELAY_SECONDS) return { error: 'The longest autorole delay is 7 days.' };
  return { seconds };
}

/** "as soon as they join" or "after 10 minutes (…)". */
export function delayText(seconds) {
  const s = clampInt(seconds, 0, MAX_AUTOROLE_DELAY_SECONDS);
  return s === 0
    ? 'as soon as they join'
    : `after **${formatDurationLong(s * 1000)}** (the delay applies to every autorole and counts from when they joined)`;
}

/**
 * Splits the configured autoroles into roles that exist and ids that no longer do, in order.
 * @param {import('discord.js').Guild} guild
 * @param {{ autoroleIds?: string[] }|null} settings
 * @returns {{ roles: import('discord.js').Role[], missingIds: string[] }}
 */
export function resolveAutoroles(guild, settings) {
  const roles = [];
  const missingIds = [];
  for (const id of new Set(settings?.autoroleIds ?? [])) {
    const role = guild.roles.cache.get(id);
    if (role) roles.push(role);
    else missingIds.push(id);
  }
  return { roles, missingIds };
}

/**
 * True while the captcha gate holds the member back: the gate is on, its role exists in the guild and
 * the member does not have it yet. A deleted captcha role never gates, so nobody is locked out.
 *
 * Bots are never gated. The captcha module skips verification for them (it tries to give them the verified
 * role and then calls assignAutorole with `force`), and it may fail to give the role (hierarchy, permissions).
 * A bot without the verified role must still get its autoroles, now or at the autorole delay, so this
 * predicate must not depend on whether that role add worked. The delayed `welcome.autorole` job re-checks
 * the gate only in its screening-watch branch; a delayed grant of a bot never re-checks anything.
 */
export function isCaptchaGated(member, settings) {
  if (member?.user?.bot === true) return false;
  return settings?.captchaEnabled === true
    && typeof settings.captchaRoleId === 'string'
    && member.guild.roles.cache.has(settings.captchaRoleId)
    && !member.roles.cache.has(settings.captchaRoleId);
}

/** When the autoroles are due: the join time plus the delay (clamped to 0..7 days). */
export function autoroleDueAt(member, settings, now = Date.now()) {
  return (member.joinedTimestamp ?? now) + clampInt(settings?.autoroleDelay, 0, MAX_AUTOROLE_DELAY_SECONDS) * 1000;
}

/**
 * Did a guildMemberUpdate complete Membership Screening?
 * 'cached' = the old member was cached and was pending; 'maybe' = the old member was not cached (partial),
 * the member is no longer pending and joined recently enough that a watch may exist; null = not a completion.
 */
export function isScreeningCompletion({ oldMember, newMember, oldKnown, now = Date.now() } = {}) {
  if (newMember?.pending !== false) return null;
  if (oldKnown) return oldMember?.pending === true ? 'cached' : null;
  const joined = newMember.joinedTimestamp;
  if (!joined || now - joined > SCREENING_WATCH_MS) return null;
  return 'maybe';
}

/** The autorole an input names among the configured ones: an id, a role mention, or a role name. */
export function resolveAutoroleInput(guild, settings, input) {
  const ids = settings?.autoroleIds ?? [];
  const raw = String(input ?? '').trim();
  if (/^\d{17,20}$/.test(raw)) return ids.includes(raw) ? raw : null;
  const mention = /^<@&(\d{17,20})>$/.exec(raw);
  if (mention) return ids.includes(mention[1]) ? mention[1] : null;
  const name = raw.replace(/^@/, '').toLowerCase();
  if (!name) return null;
  return ids.find((id) => guild.roles.cache.get(id)?.name?.toLowerCase() === name) ?? null;
}

// ── Autoroles: granting and scheduling ─────────────────────────────────────────

/**
 * Gives the roles one at a time (never the array form). Idempotent: held roles are skipped.
 * @param {import('discord.js').GuildMember} member
 * @param {import('discord.js').Role[]} roles
 * @param {{ rethrowTransient?: boolean }} [opts] let unexpected errors reach the scheduler's retry
 * @returns {Promise<{ added: string[], failed: { roleId: string, reason: string }[] }>}
 */
export async function grantAutoroles(member, roles, { rethrowTransient = false } = {}) {
  const added = [];
  const failed = [];
  const guildId = member.guild.id;
  for (const role of roles) {
    if (member.roles.cache.has(role.id)) continue;
    const chk = checkAssignableRole(member.guild, role, { allowDangerous: false });
    if (!chk.ok) {
      failed.push({ roleId: role.id, reason: chk.reason });
      warnOnce(`${guildId}:autorole:${role.id}`, `Autorole ${role.id} skipped in ${guildId}: ${chk.reason}`);
      continue;
    }
    try {
      await member.roles.add(role.id, 'Welcome autorole');
      added.push(role.id);
    } catch (err) {
      const code = err?.code;
      if (code === 10007) break; // the member left
      if (code === 10011 || code === 50013 || code === 50001) {
        failed.push({ roleId: role.id, reason: err.message });
        warnOnce(`${guildId}:autorole:${role.id}`, `Autorole ${role.id} could not be given in ${guildId}: ${err.message} (${code})`);
        continue;
      }
      if (rethrowTransient) throw err;
      failed.push({ roleId: role.id, reason: err?.message ?? 'error' });
      logger.warn(`Autorole ${role.id} failed for ${member.id} in ${guildId}: ${err?.message}`);
    }
  }
  return { added, failed };
}

// May throw (DB failure). The gates are evaluated at schedule time, so a delayed-grant job needs no flags.
async function scheduleOrGrant(member, settings, { force = false, captchaPending = false, cancelExisting = false } = {}) {
  const guild = member.guild;
  const { roles } = resolveAutoroles(guild, settings);
  if (!roles.length) return;
  const key = autoroleJobKey(guild.id, member.id);

  // Membership Screening not done: a durable watch job grants once it passes. `force` never skips this gate.
  if (member.pending === true) {
    await scheduleJob({
      guildId: guild.id, type: 'welcome.autorole', runAt: new Date(Date.now() + SCREENING_POLL_MS),
      payload: { userId: member.id, watch: true }, dedupeKey: key, onConflict: 'keep',
    });
    return;
  }
  // `force` skips only the captcha gate; captcha calls assignAutorole({ force: true }) once the member is verified.
  if (!force && (captchaPending || isCaptchaGated(member, settings))) return;

  const due = autoroleDueAt(member, settings);
  if (due <= Date.now() + 5_000) {
    await grantAutoroles(member, roles);
    if (cancelExisting) await cancelJobs({ dedupeKey: key }); // drops a screening watch or a stale delayed job
    return;
  }
  await scheduleJob({
    guildId: guild.id, type: 'welcome.autorole', runAt: new Date(due), payload: { userId: member.id },
    dedupeKey: key, onConflict: 'replace', // durable, at most 7 days out
  });
}

/**
 * The contract for the captcha package: gives (or schedules) the autoroles. Never throws.
 * `force` skips only the captcha gate; a member still pending Rules Screening is never given roles
 * here, the screening watch delivers them once screening passes.
 * @param {import('discord.js').GuildMember} member
 * @param {{ force?: boolean }} [opts]
 * @returns {Promise<void>}
 */
export async function assignAutorole(member, { force = false } = {}) {
  try {
    const settings = await getSettingsSafe(member.guild.id);
    if (!settings) return;
    await scheduleOrGrant(member, settings, { force: force === true, cancelExisting: true });
  } catch (err) {
    logger.error(`assignAutorole failed for ${member?.id} in ${member?.guild?.id}`, err);
  }
}

// ── Hooks and job body ─────────────────────────────────────────────────────────

/**
 * guildMemberAdd. Autoroles first and isolated, so message rendering can never delay or skip them;
 * the module flag is checked here because the hook itself is not gated.
 */
export async function onMemberJoin(ctx) {
  const { member, guild } = ctx;
  const settings = await ctx.getSettings();
  if (!settings) return;
  if (settings.autoroleIds?.length) {
    try {
      await scheduleOrGrant(member, settings, { captchaPending: ctx.state?.captchaPending === true });
    } catch (err) {
      logger.error(`Autorole failed for ${member.id} in ${guild.id}`, err);
    }
  }
  if (!settings.welcomeEnabled) return;
  const getCard = memoOnce(() => renderWelcomeCard(member, settings));
  await Promise.allSettled([
    sendWelcomeChannel(member, settings, getCard),
    sendWelcomeDm(member, settings, getCard),
  ]);
}

/** guildMemberRemove. Cancels pending autorole jobs always; posts the goodbye when enabled. */
export async function onMemberLeave(ctx) {
  const settings = await ctx.getSettings();
  if (!settings) return;
  if (settings.autoroleIds?.length) await cancelJobs({ dedupeKey: autoroleJobKey(ctx.guildId, ctx.user.id) });
  if (settings.welcomeEnabled && settings.goodbyeEnabled) await sendGoodbye(ctx.member, ctx.user, settings, ctx.guild);
}

/** guildMemberUpdate: Membership Screening completed (cached transition, or an uncached old member). */
export async function onMemberUpdate(ctx) {
  const kind = isScreeningCompletion({ oldMember: ctx.oldMember, newMember: ctx.newMember, oldKnown: ctx.oldKnown });
  if (!kind) return;
  const settings = await ctx.getSettings();
  if (!settings?.autoroleIds?.length) return;
  if (kind === 'maybe') {
    // Uncached old member: only act when we were watching this join, so roles a moderator removed
    // are never re-added on unrelated updates.
    const job = await getJob(autoroleJobKey(ctx.guildId, ctx.newMember.id));
    if (!job || job.payload?.watch !== true) return;
  }
  try {
    await scheduleOrGrant(ctx.newMember, settings, { cancelExisting: true });
  } catch (err) {
    logger.error(`Screening autorole failed for ${ctx.newMember.id} in ${ctx.guildId}`, err);
  }
}

/** roleDelete: prunes the deleted role from the autorole list (autoroles are not module-gated, neither is this). */
export async function onRoleDelete(ctx) {
  const s = await ctx.getSettings();
  if (!s?.autoroleIds?.includes(ctx.role.id)) return;
  await updateSettingsWith(ctx.guildId, (cur) => (cur.autoroleIds.includes(ctx.role.id)
    ? { autoroleIds: cur.autoroleIds.filter((id) => id !== ctx.role.id) }
    : null));
}

/**
 * `welcome.autorole` job: a delayed grant ({ userId }) or a screening watch ({ userId, watch: true }).
 * Re-reads the current autorole list when it fires. Idempotent.
 *
 * Captcha: the gate is evaluated when a grant is scheduled (scheduleOrGrant), not when it fires, so a plain
 * delayed grant ({ userId }) never re-checks it. Only the screening-watch branch does, because a member who just
 * passed Membership Screening may still be waiting for verification (captcha then calls assignAutorole with
 * `force` once they verify). Bots are never gated (isCaptchaGated), so their delayed grant always goes through,
 * even when captcha could not give them the verified role.
 */
export async function runAutoroleJob(job, { guild }) {
  const { userId, watch = false } = job.payload ?? {};
  if (typeof userId !== 'string' || !/^\d{17,20}$/.test(userId)) throw new PermanentJobError('welcome.autorole: invalid payload');
  const settings = await getSettings(guild.id); // a DB failure throws: the scheduler retries with back-off
  const { roles } = resolveAutoroles(guild, settings);
  if (!roles.length) return undefined;
  let member;
  try {
    member = await guild.members.fetch({ user: userId, force: true });
  } catch (err) {
    if (err?.code === 10007 || err?.code === 10013) return undefined; // the member left: done
    throw err;
  }
  if (watch) {
    if (member.pending) {
      if (Date.now() - (member.joinedTimestamp ?? 0) >= SCREENING_WATCH_MS) return undefined; // give up after 7 days
      return { nextRunAt: new Date(Date.now() + SCREENING_POLL_MS) };
    }
    // Screening completed while no event reached us (the bot was offline): evaluate now.
    if (isCaptchaGated(member, settings)) return undefined;
    const due = autoroleDueAt(member, settings);
    if (due > Date.now() + 5_000) return { nextRunAt: new Date(due), payload: { userId } }; // becomes a delayed grant
  }
  await grantAutoroles(member, roles, { rethrowTransient: true }); // permission failures do not throw (no retry)
  return undefined;
}

// ── Diagnostics ────────────────────────────────────────────────────────────────

const MODULE_OFF_LINE = '⚠️ The Welcome & Goodbye module is off (`/welcome enable`).';
const USER_TOKEN_RE = /\{user(?:\.mention)?\}/i;

function channelProblems(guild, id, label, needs, setCommand) {
  if (!id) return [`⚠️ No ${label} channel is set (\`${setCommand}\`).`];
  const channel = guild.channels.cache.get(id);
  if (!channel) return [`⚠️ The ${label} channel <#${id}> was not found. Pick another with \`${setCommand}\`.`];
  const u = checkChannelUsable(channel, { needs });
  if (u.ok) return [];
  return [`⚠️ ${u.missing.length ? `I need **${permissionNames(u.missing).join(', ')}** in <#${id}>.` : `${u.reason}.`}`];
}

/** The permissions the welcome channel needs for the current settings. */
export function welcomeChannelNeeds(settings) {
  return ['ViewChannel', 'SendMessages', ...(settings?.welcomeUseEmbed !== false ? ['EmbedLinks'] : []), ...(welcomeCardEnabled(settings) ? ['AttachFiles'] : [])];
}

/**
 * Short human lines about what stops a feature from working. Lines start with a warning sign; the
 * one tip starts with a light bulb. Pure apart from reading the guild's caches.
 * @param {import('discord.js').Guild} guild
 * @param {object} settings
 * @param {'welcome'|'dm'|'goodbye'|'autoroles'} kind
 * @returns {string[]}
 */
export function diagnoseWelcome(guild, settings, kind) {
  const out = [];
  const s = settings ?? {};
  try {
    if (kind !== 'autoroles' && !s.welcomeEnabled) out.push(MODULE_OFF_LINE);
    if (kind === 'welcome') {
      if (s.welcomeMessageEnabled === false) out.push('⚠️ The channel welcome message is turned off (`/welcome message enabled:true`).');
      out.push(...channelProblems(guild, s.welcomeChannelId, 'welcome', welcomeChannelNeeds(s), '/welcome channel'));
      const { content, embed } = resolveTemplate('welcome', s);
      if (!content && !embed && !welcomeCardEnabled(s)) out.push('⚠️ Nothing would be sent: there is no text, embed or card.');
      if (!USER_TOKEN_RE.test(content ?? '')) out.push('💡 Add `{user}` to the text so new members get pinged.');
    } else if (kind === 'dm') {
      if (!s.welcomeDmEnabled) out.push('⚠️ The welcome DM is off (`/welcome dm enable`).');
      const { content, embed } = resolveTemplate('dm', s);
      if (!content && !embed && !s.welcomeDmCard) out.push('⚠️ Nothing would be sent: the DM has no text, embed or card.');
    } else if (kind === 'goodbye') {
      if (!s.goodbyeEnabled) out.push('⚠️ The goodbye message is off (`/welcome goodbye enable`).');
      out.push(...channelProblems(guild, s.goodbyeChannelId, 'goodbye', ['ViewChannel', 'SendMessages', ...(s.goodbyeUseEmbed ? ['EmbedLinks'] : [])], '/welcome goodbye channel'));
    } else if (kind === 'autoroles') {
      const { roles, missingIds } = resolveAutoroles(guild, s);
      if (missingIds.length) out.push(`⚠️ ${missingIds.length} autorole${missingIds.length === 1 ? '' : 's'} no longer exist${missingIds.length === 1 ? 's' : ''} and will be skipped: ${missingIds.map((id) => `\`${id}\``).join(', ')}.`);
      for (const role of roles) {
        const chk = checkAssignableRole(guild, role, { allowDangerous: false });
        if (!chk.ok) out.push(`⚠️ <@&${role.id}>: ${chk.reason}`);
      }
      if (!s.welcomeEnabled && (s.autoroleIds?.length ?? 0) > 0) out.push('⚠️ Autoroles run even while the module is off.');
    }
  } catch (err) {
    logger.warn(`diagnoseWelcome failed: ${err?.message}`);
  }
  return out;
}

/** "Unknown variables: `{usr}`" for any text that contains tokens nothing knows; '' when all are fine. */
export function unknownTokenWarning(text, ctx) {
  try {
    const { unknownTokens } = renderPlaceholdersDetailed(text, ctx);
    if (!unknownTokens.length) return '';
    return `Unknown variables: ${joinWithLimit(unknownTokens.map((t) => `\`${t}\``), { max: 500 })}`;
  } catch {
    return '';
  }
}

/** Notes shown with the autoroles: the captcha gate and Membership Screening hold roles back. */
export function autoroleNotes(guild, settings) {
  const notes = [];
  if (settings?.captchaEnabled) notes.push('Members get autoroles after they pass verification.');
  if (guild?.features?.includes('MEMBER_VERIFICATION_GATE_ENABLED')) notes.push('Members get autoroles after they accept the server rules.');
  return notes;
}

// ── Settings changes used by the commands ──────────────────────────────────────

/**
 * Edits one of the three message texts and, for the welcome text, its sub-toggle.
 * @param {string} guildId
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {{ text?: string|null, reset?: boolean, enabled?: boolean|null }} opts
 * @returns {Promise<object|null>} the new settings, or null when there was nothing to change
 * @throws {WelcomeConfigError}
 */
export async function updateMessage(guildId, kind, { text = null, reset = false, enabled = null } = {}) {
  if (text !== null && reset) throw new WelcomeConfigError('Use either text or reset, not both.');
  const field = MESSAGE_FIELDS[kind];
  const data = {};
  if (text !== null) {
    const v = validateTemplate({ content: text }, { requireSomething: false });
    if (!v.ok) throw new WelcomeConfigError(joinWithLimit(v.errors, { sep: '\n', max: 1000 }));
    data[field] = text;
  } else if (reset) {
    data[field] = null;
  }
  if (enabled !== null && kind === 'welcome') data.welcomeMessageEnabled = enabled;
  if (!Object.keys(data).length) return null;
  return saveSettings(guildId, data);
}

const THUMBNAILS = Object.freeze({ avatar: '{user.avatar}', server: '{server.icon}', none: null });
const PLACEHOLDER_IMAGES = new Set(['{user.avatar}', '{user.avatar_url}', '{server.icon}', '{server.icon_url}']);

/**
 * `/welcome embed` and `/welcome goodbye embed`. `imageUrl` is welcome-only.
 * @param {string} guildId
 * @param {'welcome'|'goodbye'} kind
 * @param {{ enabled?: boolean|null, thumbnail?: 'avatar'|'server'|'none'|null, imageUrl?: string|null, reset?: boolean }} opts
 * @returns {Promise<object>} the new settings
 * @throws {WelcomeConfigError}
 */
export async function updateEmbedOptions(guildId, kind, { enabled = null, thumbnail = null, imageUrl = null, reset = false } = {}) {
  const imageGiven = imageUrl !== null && kind === 'welcome';
  if (enabled === null && thumbnail === null && !imageGiven && !reset) throw new WelcomeConfigError('Choose at least one option.');
  if (reset && (thumbnail !== null || imageGiven)) throw new WelcomeConfigError('Use reset on its own.');

  let image = null;
  if (imageGiven) {
    const raw = String(imageUrl).trim();
    if (raw.toLowerCase() === 'none') image = null;
    else if (isHttpUrl(raw) || PLACEHOLDER_IMAGES.has(raw)) image = raw;
    else throw new WelcomeConfigError('The image must be an http(s) URL, {user.avatar}, {server.icon} or none.');
  }

  const embedField = EMBED_FIELDS[kind];
  const useField = kind === 'welcome' ? 'welcomeUseEmbed' : 'goodbyeUseEmbed';
  const fallback = kind === 'welcome' ? DEFAULT_WELCOME_EMBED : DEFAULT_GOODBYE_EMBED;

  return saveSettingsWith(guildId, (cur) => {
    const data = {};
    if (reset) {
      data[embedField] = null;
    } else if (thumbnail !== null || imageGiven) {
      const base = healEmbed(cur[embedField]) ?? structuredClone(fallback);
      if (thumbnail !== null) base.thumbnail = THUMBNAILS[thumbnail];
      if (imageGiven) base.image = image;
      data[embedField] = serializeEmbed(base);
    }
    if (enabled !== null) data[useField] = enabled;
    // Only a welcome embed write needs the card flag preserved and the text frozen.
    return kind === 'welcome' && embedField in data ? materialiseWelcome(cur, data) : data;
  });
}

/** Parses a colour option into '#rrggbb', or throws a WelcomeConfigError naming the option. */
function colorOption(label, value) {
  const n = parseColor(value);
  if (n === null) throw new WelcomeConfigError(`${label} is not a colour. Use a hex code like #5865F2 or a name like blurple.`);
  return formatColor(n).toLowerCase();
}

/**
 * `/welcome card` with options: validates everything first (the background image is downloaded once),
 * then writes the style. Colour options become '#rrggbb'.
 * @param {string} guildId
 * @param {{ enabled?: boolean|null, background?: string|null, backgroundUrl?: string|null, textColor?: string|null,
 *   accentColor?: string|null, overlay?: number|null, reset?: boolean }} opts
 * @returns {Promise<object>} the new settings
 * @throws {WelcomeConfigError}
 */
export async function updateCardStyle(guildId, {
  enabled = null, background = null, backgroundUrl = null, textColor = null, accentColor = null, overlay = null, reset = false,
} = {}) {
  const patch = {};
  if (background !== null) patch.background = colorOption('Background', background);
  if (textColor !== null) patch.textColor = colorOption('Text colour', textColor);
  if (accentColor !== null) patch.accentColor = colorOption('Accent colour', accentColor);
  if (overlay !== null) patch.overlayOpacity = Math.round(Math.min(1, Math.max(0, Number(overlay))) * 100) / 100;
  if (backgroundUrl !== null) {
    if (String(backgroundUrl).trim().toLowerCase() === 'none') {
      patch.backgroundUrl = null;
    } else {
      const url = String(backgroundUrl).trim();
      const r = await validateCardBackground(url);
      if (!r.ok) throw new WelcomeConfigError(r.reason, 'Image Not Loaded');
      patch.backgroundUrl = url;
    }
  }
  return saveSettingsWith(guildId, (cur) => {
    const base = reset
      ? { enabled: welcomeCardEnabled(cur), background: null, backgroundUrl: null, textColor: null, accentColor: null, overlayOpacity: null }
      : { ...parseCardStyle(cur.welcomeCardStyle), enabled: welcomeCardEnabled(cur) };
    const next = { ...base, ...patch };
    if (enabled !== null) next.enabled = enabled; // applied last
    return { welcomeCardStyle: serializeCardStyle(next) };
  });
}

/** One-line description of the card style for status and replies. */
export function describeCardStyle(style) {
  const s = style ?? {};
  const set = s.background || s.backgroundUrl || s.textColor || s.accentColor || s.overlayOpacity !== null;
  if (!set) return 'Default';
  return `background ${s.background ?? 'default'}, image ${s.backgroundUrl ? 'yes' : 'no'}, text ${s.textColor ?? 'default'}, accent ${s.accentColor ?? 'default'}, overlay ${s.overlayOpacity ?? 0.5}`;
}

/**
 * Adds an autorole. The delay applies to every autorole; it is written only when given (R074).
 * @param {{ guild: object, executor: object, role: object, delay?: string|null }} opts
 * @returns {Promise<{ settings: object, already: boolean }>}
 * @throws {WelcomeConfigError}
 */
export async function addAutorole({ guild, executor, role, delay = null }) {
  const chk = checkAssignableRole(guild, role, { executor, allowDangerous: false });
  if (!chk.ok) {
    let hint = '';
    if (chk.dangerous?.length) hint = ' Autoroles are given to everyone who joins, so roles with these permissions are refused.';
    else if (/highest role/i.test(chk.reason ?? '')) hint = ' Move my highest role above it in **Server Settings → Roles**.';
    throw new WelcomeConfigError(`${chk.reason}${hint}`, 'Cannot Use This Role');
  }
  let seconds = null;
  if (delay !== null && delay !== undefined) {
    const d = parseDelayInput(delay);
    if ('error' in d) throw new WelcomeConfigError(d.error);
    seconds = d.seconds;
  }
  let already = false;
  const settings = await saveSettingsWith(guild.id, (cur) => {
    if (cur.autoroleIds.includes(role.id)) {
      if (seconds === null) throw new WelcomeConfigError(`<@&${role.id}> is already an autorole.`, 'Already Added');
      already = true;
      return { autoroleDelay: seconds };
    }
    if (cur.autoroleIds.length >= LIMITS.AUTOROLES) throw new WelcomeConfigError(`You can have up to ${LIMITS.AUTOROLES} autoroles.`);
    return { autoroleIds: [...cur.autoroleIds, role.id], ...(seconds !== null && { autoroleDelay: seconds }) };
  });
  return { settings, already };
}

/**
 * Removes an autorole (deleted roles included).
 * @returns {Promise<{ settings: object|null, id: string|null }>} id null = the input is not an autorole
 */
export async function removeAutorole(guild, settings, input) {
  const id = resolveAutoroleInput(guild, settings, input);
  if (!id) return { settings: null, id: null };
  const after = await saveSettingsWith(guild.id, (cur) => (cur.autoroleIds.includes(id)
    ? { autoroleIds: cur.autoroleIds.filter((x) => x !== id) }
    : null));
  return { settings: after, id };
}

/** Writes the autorole delay. @throws {WelcomeConfigError} */
export async function setAutoroleDelay(guildId, input) {
  const d = parseDelayInput(input);
  if ('error' in d) throw new WelcomeConfigError(d.error);
  return { settings: await saveSettings(guildId, { autoroleDelay: d.seconds }), seconds: d.seconds };
}

/** Removes autorole ids whose roles no longer exist (R230). Returns how many were pruned. */
export async function pruneMissingAutoroles(guild, missingIds) {
  if (!missingIds.length) return 0;
  const gone = new Set(missingIds);
  await updateSettingsWith(guild.id, (cur) => {
    const keep = cur.autoroleIds.filter((id) => !gone.has(id) || guild.roles.cache.has(id));
    return keep.length === cur.autoroleIds.length ? null : { autoroleIds: keep };
  });
  return missingIds.length;
}

// ── Status ─────────────────────────────────────────────────────────────────────

const oneLine = (text) => String(text ?? '').replace(/\s+/g, ' ').trim();
const quote = (text) => (text ? `"${clampText(oneLine(text), 300)}"` : '*(no text)*');

/** The mode line of a message, e.g. "Text + embed (custom)". */
export function describeMode(kind, settings) {
  const s = settings ?? {};
  if (kind === 'welcome') {
    return s.welcomeUseEmbed === false ? 'Text only' : `Text + embed (${healEmbed(s.welcomeEmbed) ? 'custom' : 'default'})`;
  }
  if (kind === 'goodbye') {
    return s.goodbyeUseEmbed ? `Embed (${healEmbed(s.goodbyeEmbed) ? 'custom' : 'default'})` : 'Text only';
  }
  return healEmbed(s.welcomeDmEmbed) ? 'Text + embed' : 'Text';
}

/**
 * The /welcome status embed.
 * @param {import('discord.js').Guild} guild
 * @param {object} settings
 */
export function buildStatusEmbed(guild, settings) {
  const s = settings ?? {};
  const onOff = (b) => (b ? 'On' : 'Off');
  const field = (name, value) => ({ name: clampText(name, 256), value: clampText(value, 1024) || '—', inline: false });
  const fields = [];

  fields.push(field('Module', s.welcomeEnabled ? '✅ On' : '❌ Off — `/welcome enable`'));

  const welcome = resolveTemplate('welcome', s);
  fields.push(field('Channel message', [
    `${onOff(s.welcomeMessageEnabled !== false)} · ${channelLabel(s.welcomeChannelId)}`,
    describeMode('welcome', s),
    `Card ${onOff(welcomeCardEnabled(s))}`,
    quote(welcome.content),
  ].join('\n')));

  const dm = resolveTemplate('dm', s);
  fields.push(field('Welcome DM', [
    `${onOff(s.welcomeDmEnabled)} · Card ${onOff(s.welcomeDmCard)}`,
    `${quote(dm.content)}${dm.embed ? ' + embed' : ''}`,
  ].join('\n')));

  const bye = resolveTemplate('goodbye', s);
  fields.push(field('Goodbye', [
    `${onOff(s.goodbyeEnabled)} · ${channelLabel(s.goodbyeChannelId)}`,
    describeMode('goodbye', s),
    quote(bye.content),
  ].join('\n')));

  fields.push(field('Card style', describeCardStyle(parseCardStyle(s.welcomeCardStyle))));

  const ids = s.autoroleIds ?? [];
  const delaySeconds = clampInt(s.autoroleDelay, 0, MAX_AUTOROLE_DELAY_SECONDS);
  fields.push(field('Autoroles', ids.length
    ? `${joinWithLimit(ids.map((id) => `<@&${id}>`), { max: 900 })}
Delay: ${delaySeconds === 0 ? 'immediately' : `${formatDurationLong(delaySeconds * 1000)} after joining`}`
    : 'None'));

  const problems = [...new Set([
    ...diagnoseWelcome(guild, s, 'welcome'),
    ...diagnoseWelcome(guild, s, 'dm'),
    ...diagnoseWelcome(guild, s, 'goodbye'),
    ...diagnoseWelcome(guild, s, 'autoroles'),
  ])];
  if (problems.length) fields.push(field('Problems', joinWithLimit(problems, { sep: '\n', max: 1024 })));

  return applyGuildColor(infoEmbed('Welcome & Goodbye'), s).addFields(fields);
}

// ── Setup modals ───────────────────────────────────────────────────────────────

function setupBase(kind, settings) {
  const s = settings ?? {};
  if (kind === 'dm') {
    return { content: s.welcomeDmMessage ?? DEFAULT_WELCOME_DM_MESSAGE, embed: healEmbed(s.welcomeDmEmbed), buttons: [] };
  }
  if (kind === 'goodbye') {
    return {
      content: resolveTemplate('goodbye', s).content ?? '',
      embed: healEmbed(s.goodbyeEmbed) ?? structuredClone(DEFAULT_GOODBYE_EMBED),
      buttons: [],
    };
  }
  return {
    content: resolveTemplate('welcome', s).content ?? '',
    embed: healEmbed(s.welcomeEmbed) ?? structuredClone(DEFAULT_WELCOME_EMBED),
    buttons: [],
  };
}

const MODAL_TITLES = Object.freeze({ welcome: 'Welcome message', dm: 'Welcome DM', goodbye: 'Goodbye message' });

/**
 * The setup form, prefilled from what is actually sent. The builder clamps every prefill to its input's
 * maximum and drops an invalid colour, so Discord can never reject the form.
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {object} settings
 * @returns {import('discord.js').ModalBuilder}
 */
export function buildSetupModal(kind, settings) {
  return buildTemplateModal({ customId: MODAL_IDS[kind], title: MODAL_TITLES[kind], template: setupBase(kind, settings) });
}

/** True when the submitted form lacks any of today's input ids (a form opened before the upgrade). */
export function isStaleSetupForm(interaction) {
  try {
    const present = new Set(interaction.fields.fields.keys());
    return !Object.values(TEMPLATE_FIELD_IDS).every((id) => present.has(id));
  } catch {
    return true;
  }
}

const SUBMIT_TITLES = Object.freeze({ welcome: 'Welcome Message Saved', dm: 'Welcome DM Saved', goodbye: 'Goodbye Message Saved' });
const PREVIEW_COMMANDS = Object.freeze({ welcome: '/welcome test', dm: '/welcome test type:dm', goodbye: '/welcome goodbye test' });

/**
 * Submit handler of the three setup modals. Replies publicly (admin convention).
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {import('discord.js').ModalSubmitInteraction} interaction
 */
export async function handleSetupModalSubmit(kind, interaction) {
  await interaction.deferReply();
  let settings;
  try {
    settings = await getSettings(interaction.guildId);
  } catch (err) {
    logger.warn(`Setup form could not load settings for ${interaction.guildId}: ${err?.message}`);
    return interaction.editReply({ embeds: [errorEmbed('Temporarily Unavailable', 'Settings could not be loaded. Nothing was saved.')] });
  }
  if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) {
    return interaction.editReply({ embeds: [errorEmbed('Insufficient Permissions', 'You need the Admin level to change these settings.')] });
  }
  // Mandatory: a form opened before the upgrade carries the retired ids and must never parse as "everything empty".
  if (isStaleSetupForm(interaction)) {
    return interaction.editReply({ embeds: [errorEmbed('Form Expired', 'This form is out of date. Run the command again.')] });
  }

  let template;
  let errors;
  try {
    // An emptied form is allowed (the warnings below explain what is sent), so the empty-message check is off.
    ({ template, errors } = parseTemplateModal(interaction, setupBase(kind, settings), { requireSomething: false }));
  } catch {
    return interaction.editReply({ embeds: [errorEmbed('Form Expired', 'This form is out of date. Run the command again.')] });
  }
  // Only real problems block the save.
  if (errors.length) {
    return interaction.editReply({ embeds: [errorEmbed('Not Saved', joinWithLimit(errors, { sep: '\n', max: 4000 }))] });
  }

  const embedJson = hasEmbedContent(template.embed) ? serializeEmbed(template.embed) : null;
  const content = template.content ?? '';
  let after;
  if (kind === 'welcome') {
    after = await saveSettingsWith(interaction.guildId, (cur) => materialiseWelcome(cur, { welcomeMessage: content, welcomeEmbed: embedJson }));
  } else if (kind === 'dm') {
    after = await saveSettings(interaction.guildId, { welcomeDmMessage: content, welcomeDmEmbed: embedJson });
  } else {
    after = await saveSettings(interaction.guildId, { goodbyeMessage: content, goodbyeEmbed: embedJson });
  }

  const warnings = [];
  const guild = interaction.guild;
  const ctx = { member: interaction.member, guild, channel: interaction.channel };
  const unknown = unknownTokenWarning(
    [template.content, template.embed?.title, template.embed?.description, template.embed?.footer?.text].filter(Boolean).join('\n'),
    ctx,
  );
  if (unknown) warnings.push(unknown);
  if (kind === 'welcome') {
    if (!embedJson && after.welcomeUseEmbed !== false) warnings.push('The embed is empty, so the default embed is used. For text only, run `/welcome embed enabled:false`.');
    if (embedJson && after.welcomeUseEmbed === false) warnings.push('Embed saved, but the welcome is in text-only mode (`/welcome embed enabled:true`).');
    if (after.welcomeMessageEnabled === false) warnings.push('The channel welcome message is turned off (`/welcome message enabled:true`).');
  } else if (kind === 'goodbye') {
    if (!embedJson && after.goodbyeUseEmbed) warnings.push('The embed is empty, so the default embed is used. For text only, run `/welcome goodbye embed enabled:false`.');
    if (!after.goodbyeEnabled) warnings.push('The goodbye message is off (`/welcome goodbye enable`).');
  } else if (!after.welcomeDmEnabled) {
    warnings.push('The welcome DM is off (`/welcome dm enable`).');
  }
  if (!after.welcomeEnabled) warnings.push('The Welcome & Goodbye module is off (`/welcome enable`).');
  const resolved = resolveTemplate(kind, after);
  if (!resolved.content && !resolved.embed && !(kind === 'dm' ? after.welcomeDmCard : kind === 'welcome' && welcomeCardEnabled(after))) {
    warnings.push('Nothing would be sent with these settings.');
  }

  const description = `Preview it with \`${PREVIEW_COMMANDS[kind]}\`.${warnings.length ? `\n\n${warnings.map((w) => `⚠️ ${w}`).join('\n')}` : ''}`;
  return interaction.editReply({ embeds: [successEmbed(SUBMIT_TITLES[kind], description)] });
}

// ── Test previews ──────────────────────────────────────────────────────────────

/**
 * `/welcome test` and `/welcome goodbye test`: the exact message a join or leave would produce, rendered for the
 * invoker, with no pings. Uses the same payload builder as real events.
 * @param {import('discord.js').ChatInputCommandInteraction} interaction
 * @param {object} settings
 * @param {'welcome'|'dm'|'goodbye'} kind
 */
export async function buildTestPreview(interaction, settings, kind) {
  await interaction.deferReply();
  const { guild, member } = interaction;
  const configured = await resolveChannel(guild, kind === 'goodbye' ? settings.goodbyeChannelId : settings.welcomeChannelId).catch(() => null);
  const channel = configured ?? interaction.channel;

  let card = null;
  const cardOn = kind === 'welcome' ? welcomeCardEnabled(settings) : kind === 'dm' ? settings.welcomeDmCard === true : false;
  if (cardOn) card = await renderWelcomeCard(member, settings, { preview: true });

  const payload = buildWelcomePayload(kind, { settings, member, user: member.user, guild, channel, card, preview: true });
  if (!payload) {
    await interaction.editReply({
      embeds: [warnEmbed('Nothing To Send', 'With the current settings nothing would be sent. Set a text with the message subcommand or turn on the embed or card.')],
    });
  } else {
    await interaction.editReply(payload);
  }
  const problems = diagnoseWelcome(guild, settings, kind);
  if (problems.length) {
    await interaction.followUp({ embeds: [warnEmbed('Setup Problems', joinWithLimit(problems, { sep: '\n', max: 4000 }))] });
  }
}

// ── Command helpers (shared by /welcome and /welcome goodbye) ───────────────────────────

/**
 * Settings for a reply that must be acknowledged within 3 seconds (a modal or a test preview): the memo
 * first, otherwise a lookup bounded by the remaining acknowledgement budget. null = unavailable.
 * @param {import('discord.js').RepliableInteraction} interaction
 * @returns {Promise<object|null>}
 */
export async function settingsForAck(interaction) {
  return peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'welcome setup').catch(() => null);
}

/**
 * Checks a channel chosen in a command: the bot must be able to use it for this feature.
 * @param {import('discord.js').Guild} guild
 * @param {{ id: string }} picked the channel option
 * @param {string[]} needs PermissionFlagsBits keys
 * @returns {Promise<{ ok: true, channel: object } | { ok: false, message: string }>}
 */
export async function checkConfigChannel(guild, picked, needs) {
  const channel = guild.channels.cache.get(picked.id) ?? await guild.channels.fetch(picked.id).catch(() => null);
  const u = checkChannelUsable(channel, { needs });
  if (u.ok) return { ok: true, channel };
  return {
    ok: false,
    message: u.missing.length
      ? `I need **${permissionNames(u.missing).join(', ')}** in <#${picked.id}>. Fix the channel permissions and run this again.`
      : `${u.reason}.`,
  };
}

const describeImage = (v) => {
  if (!v) return 'none';
  if (v === '{user.avatar}' || v === '{user.avatar_url}') return 'member avatar';
  if (v === '{server.icon}' || v === '{server.icon_url}') return 'server icon';
  return clampText(v, 100);
};

/** The mode and the images of the welcome or goodbye embed, as markdown lines. */
export function describeEmbedSettings(kind, settings) {
  const s = settings ?? {};
  const spec = kind === 'goodbye'
    ? (healEmbed(s.goodbyeEmbed) ?? DEFAULT_GOODBYE_EMBED)
    : (healEmbed(s.welcomeEmbed) ?? DEFAULT_WELCOME_EMBED);
  return [
    `**Mode:** ${describeMode(kind, s)}`,
    `**Thumbnail:** ${describeImage(spec.thumbnail)}`,
    kind === 'welcome' ? `**Image:** ${describeImage(spec.image)}` : null,
  ].filter(Boolean).join('\n');
}

const codeBlock = (text) => `\`\`\`\n${clampText(String(text).replaceAll('```', "'''"), 1000)}\n\`\`\``;

/** `/… message` without options: the mode and the effective text. */
export function messageInfoEmbed(title, kind, settings) {
  const { content } = resolveTemplate(kind, settings);
  const lines = [];
  if (kind === 'welcome') lines.push(`**Channel message:** ${settings?.welcomeMessageEnabled === false ? 'Off' : 'On'}`);
  lines.push(`**Mode:** ${describeMode(kind, settings)}`, content ? codeBlock(content) : '*(no text)*');
  return infoEmbed(title, lines.join('\n'));
}

/**
 * `/… message` with options: what changed, the text now in effect, unknown variables and tips.
 * @param {string} title
 * @param {'welcome'|'dm'|'goodbye'} kind
 * @param {object} after settings after the write
 * @param {{ text?: string|null, reset?: boolean, enabled?: boolean|null }} change
 * @param {object} ctx placeholder context for the unknown-variable check
 */
export function messageUpdateEmbed(title, kind, after, { text = null, reset = false, enabled = null } = {}, ctx = {}) {
  const lines = [];
  if (text !== null) lines.push('The text was saved.');
  if (reset) lines.push('The text was reset to the default.');
  if (enabled !== null && kind === 'welcome') lines.push(`The channel welcome message is now **${enabled ? 'on' : 'off'}**.`);
  const { content } = resolveTemplate(kind, after);
  lines.push(content ? codeBlock(content) : '*(no text)*');
  const unknown = text !== null ? unknownTokenWarning(text, ctx) : '';
  if (unknown) lines.push(`⚠️ ${unknown}`);
  if (kind === 'welcome' && !USER_TOKEN_RE.test(content ?? '')) {
    lines.push('💡 Add `{user}` to the text so new members get pinged.');
  }
  if (kind === 'goodbye' && USER_TOKEN_RE.test(content ?? '')) {
    lines.push("⚠️ Mentions don't notify members who left; `{user.name}` or `{user.idname}` reads better.");
  }
  return successEmbed(title, lines.join('\n'));
}

/** The card settings as markdown lines. */
export function cardSummary(settings) {
  const style = parseCardStyle(settings?.welcomeCardStyle);
  return [
    `**Card:** ${welcomeCardEnabled(settings) ? 'On' : 'Off'}`,
    `**Background colour:** ${style.background ?? 'default'}`,
    `**Background image:** ${style.backgroundUrl ? 'set' : 'none'}`,
    `**Text colour:** ${style.textColor ?? 'default'}`,
    `**Accent colour:** ${style.accentColor ?? 'default'}`,
    `**Overlay:** ${style.overlayOpacity ?? 'default (0.5 over an image)'}`,
  ].join('\n');
}

/**
 * An embed plus a preview of the card rendered for the invoker (omitted when it fails).
 * @param {import('discord.js').EmbedBuilder} embed
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings
 * @returns {Promise<{ embeds: object[], files?: object[] }>}
 */
export async function withCardPreview(embed, member, settings) {
  const buf = await renderWelcomeCard(member, settings, { preview: true });
  if (!buf) return { embeds: [embed] };
  return {
    embeds: [embed.setImage(`attachment://${CARD_FILE_NAME}`)],
    files: [new AttachmentBuilder(buf, { name: CARD_FILE_NAME })],
  };
}

/** Warns when the card is on and the configured welcome channel cannot take attachments. */
export function cardChannelWarning(guild, settings) {
  if (!welcomeCardEnabled(settings) || !settings?.welcomeChannelId) return '';
  const channel = guild.channels.cache.get(settings.welcomeChannelId);
  if (!channel || missingBotPermissions(channel, ['AttachFiles']).length === 0) return '';
  return `⚠️ I cannot attach files in <#${settings.welcomeChannelId}>, so the card is left out until I get **Attach Files** there.`;
}

/** Lines for the autorole list: the delay, one line per role (with a problem when it cannot be given), notes. */
export function autoroleListLines(guild, settings, { prunedCount = 0 } = {}) {
  const { roles } = resolveAutoroles(guild, settings);
  const seconds = clampInt(settings?.autoroleDelay, 0, MAX_AUTOROLE_DELAY_SECONDS);
  const lines = [`**Delay:** ${seconds === 0 ? 'immediately' : `${formatDurationLong(seconds * 1000)} after joining (applies to every autorole)`}`, ''];
  for (const role of roles) {
    const chk = checkAssignableRole(guild, role, { allowDangerous: false });
    lines.push(`<@&${role.id}>${chk.ok ? '' : ` — ⚠️ ${chk.reason}`}`);
  }
  if (prunedCount > 0) lines.push('', `Removed ${prunedCount} deleted role${prunedCount === 1 ? '' : 's'} from the list.`);
  const notes = autoroleNotes(guild, settings);
  if (notes.length) lines.push('', ...notes);
  return lines;
}
