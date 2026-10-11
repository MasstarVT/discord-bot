// The message editor of the web dashboard: a MessageTemplate / EmbedSpec editor with a live, Discord-style preview.
//   createTemplateEditor(desc, opts) -> FieldControl   (descriptor types 'template' and 'embed', see src/dashboard/schema.js)
//   renderMessagePreview(template, ctx) -> HTMLElement
// Limits and checks mirror validateTemplate in src/utils/customEmbed.js, so a save is not refused by the server.
// The preview is built from DOM nodes only (never parsed markup); placeholders are shown with sample values.
//
// Import-safe in plain Node: nothing touches document, window or storage until a function is called.

import { h, icon, ICONS, createSwitch, paintSwitch } from './lib.js';

/** Discord's message and embed limits (the LIMITS of src/utils/customEmbed.js). */
export const MESSAGE_LIMITS = Object.freeze({
  content: 2000, title: 256, description: 4096, fieldName: 256, fieldValue: 1024,
  fields: 25, footer: 2048, author: 256, total: 6000, buttons: 25, buttonLabel: 80, url: 2048, buttonUrl: 512,
});

const L = MESSAGE_LIMITS;
const DEFAULT_EMBED_COLOR = 0x5865f2; // COLORS.INFO: what the bot uses when an embed has no colour of its own
const BUTTONS_PER_ROW = 5;
const MAX_BUTTON_ROWS = 5;
const IMAGE_HOSTS = new Set(['cdn.discordapp.com']); // the dashboard CSP's img-src (besides 'self' and data:)
const TOKEN_RE = /\{([^{}\n]{1,64})\}/g;
const ATTACHMENT_RE = /^attachment:\/\/[^\s/]{1,200}$/;
const CUSTOM_EMOJI_RE = /^<(a?):(\w{2,32}):(\d{17,20})>$/;
const HEX_RE = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i;
const EMPTY_MESSAGE = 'The message is empty. Add some text or an embed.';
const SAMPLE = Object.freeze({
  user: '100000000000000001',
  owner: '100000000000000002',
  channel: '100000000000000003',
  role: '100000000000000004',
  avatar: 'https://cdn.discordapp.com/embed/avatars/0.png',
});

/* ------------------------------------------------------------------ */
/* Small helpers                                                        */
/* ------------------------------------------------------------------ */

const SVG_NS = 'http://www.w3.org/2000/svg';
// Extra stroke icons (24x24 grid, sub-paths joined with "|"); the shared ones come from lib.js.
const EXTRA_ICONS = {
  up: 'M18 15l-6-6-6 6',
  down: 'M6 9l6 6 6-6',
  trash: 'M3 6h18|M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2|M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6|M10 11v6|M14 11v6',
  external: 'M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6|M15 3h6v6|M10 14L21 3',
  image: 'M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z'
    + '|M8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z|M21 15l-5-5L5 21',
  braces: 'M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5a2 2 0 0 0 2 2h1'
    + '|M16 3h1a2 2 0 0 1 2 2v5a2 2 0 0 0 2 2 2 2 0 0 0-2 2v5a2 2 0 0 1-2 2h-1',
  pencil: 'M12 20h9|M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z',
  send: 'M22 2L11 13|M22 2l-7 20-4-9-9-4z',
  eye: 'M1 12s4-8 11-8 11 8 11 8-4 8-11 8S1 12 1 12z|M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z',
  hash: 'M4 9h16|M4 15h16|M10 3L8 21|M16 3l-2 18',
  undo: 'M3 7v6h6|M3 13a9 9 0 1 0 2.6-6.4L3 9',
  layers: 'M12 2l10 5-10 5L2 7z|M2 17l10 5 10-5|M2 12l10 5 10-5',
};

/**
 * An icon from lib.js, or one of the editor's extra icons.
 * @param {string} name
 * @returns {SVGElement}
 */
export function uiIcon(name) {
  if (Object.hasOwn(ICONS, name)) return icon(name);
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  for (const d of (EXTRA_ICONS[name] || '').split('|')) {
    if (!d) continue;
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

const isObj = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
const str = (v) => (typeof v === 'string' ? v : typeof v === 'number' && Number.isFinite(v) ? String(v) : '');
const isInt24 = (n) => Number.isInteger(n) && n >= 0 && n <= 0xffffff;
const fmt = (n) => Number(n).toLocaleString('en-US');

/** '#RRGGBB' of a colour integer. */
export function colorHex(n) {
  return `#${(isInt24(n) ? n : DEFAULT_EMBED_COLOR).toString(16).padStart(6, '0').toUpperCase()}`;
}

function parseHex(s) {
  const m = HEX_RE.exec(String(s ?? '').trim());
  if (!m) return null;
  const hex = m[1].length === 3 ? [...m[1]].map((c) => c + c).join('') : m[1];
  return parseInt(hex, 16);
}

// Discord draws a colour of 0 as "no colour", so black is stored as #010101 (like the bot's colour names do).
const storableColor = (n) => (n === 0 ? 0x010101 : n);

/**
 * True for an absolute http(s) URL of at most 2048 characters without whitespace (customEmbed.isHttpUrl).
 * @param {unknown} s
 * @returns {boolean}
 */
export function isHttpUrl(s) {
  if (typeof s !== 'string' || s.length > L.url || /\s/.test(s)) return false;
  try {
    const u = new URL(s);
    return (u.protocol === 'http:' || u.protocol === 'https:') && u.hostname !== '';
  } catch {
    return false;
  }
}

let emojiRe; // built on first use: the 'v' flag is missing in older browsers
function isUnicodeEmoji(s) {
  if (emojiRe === undefined) {
    try {
      emojiRe = new RegExp('^(?:\\p{RGI_Emoji}|\\p{Extended_Pictographic}\\uFE0F?)$', 'v');
    } catch {
      try {
        const pic = '\\p{Extended_Pictographic}';
        emojiRe = new RegExp(`^${pic}(?:\\uFE0F|\\p{Emoji_Modifier}|\\u200D${pic})*$`, 'u');
      } catch {
        emojiRe = null;
      }
    }
  }
  return emojiRe ? emojiRe.test(s) : s.length <= 16;
}

/** True for one Unicode emoji or a custom emoji '<:name:id>' (what a link button accepts). */
export function isButtonEmoji(s) {
  const v = String(s ?? '').trim();
  return CUSTOM_EMOJI_RE.test(v) || (v.length <= 16 && isUnicodeEmoji(v));
}

let uidSeq = 0;
const nextUid = () => {
  uidSeq += 1;
  return `i${uidSeq}`;
};

/* ------------------------------------------------------------------ */
/* Placeholders and sample values                                       */
/* ------------------------------------------------------------------ */

// Used when the catalog has no placeholder list (built-ins of src/utils/placeholderParser.js, most useful first).
const FALLBACK_PLACEHOLDERS = [
  ['{user}', 'Mention of the member', 'user'],
  ['{user.name}', 'Username', 'user'],
  ['{user.display_name}', 'Server nickname, falling back to the display name', 'user'],
  ['{user.id}', 'User ID', 'user'],
  ['{user.avatar}', 'Avatar URL', 'user'],
  ['{user.created}', 'Account age, relative', 'user'],
  ['{user.joined}', 'Time since joining the server, relative', 'user'],
  ['{server}', 'Server name', 'server'],
  ['{server.member_count}', 'Member count, e.g. 1,234', 'server'],
  ['{server.member_count.ordinal}', 'Member count as an ordinal, e.g. 1,234th', 'server'],
  ['{server.icon}', 'Server icon URL', 'server'],
  ['{server.owner}', 'Mention of the server owner', 'server'],
  ['{server.boost_count}', 'Number of boosts', 'server'],
  ['{channel}', 'Mention of the channel', 'channel'],
  ['{channel.name}', 'Channel name', 'channel'],
  ['{random}', 'Random number from 0 to 10', 'other'],
  ['{random:1:100}', 'Random number from x to y', 'other'],
].map(([token, description, group]) => ({ token, description, group }));

const FEATURED = ['{user}', '{user.name}', '{server}', '{server.member_count}', '{channel}'];

// Descriptions of feature variables (descriptor `placeholders`); anything else is "Supported by this message".
const FEATURE_DESCRIPTIONS = {
  '{player}': 'Mention of the member', '{player.name}': 'Display name of the member', '{level}': 'The new level',
  '{rank}': 'Leaderboard rank', '{xp}': 'Total XP', '{mention}': 'Mention of the member', '{age}': 'Age (when the year is known)',
  '{count}': 'The counted number', '{role}': 'The role', '{number}': 'Ticket number', '{topic}': 'Ticket topic',
  '{reason}': 'The reason given', '{item}': 'Item name', '{quantity}': 'Quantity', '{coins}': 'Coins', '{price}': 'Price',
  '{currency}': 'Currency name', '{title}': 'Post title', '{url}': 'Post link', '{link}': 'Post link', '{author}': 'Post author',
  '{feed}': 'Feed name', '{achievement}': 'Achievement and tier', '{tier}': 'Tier name', '{goal}': 'Tier goal',
  '{progress}': 'Progress', '{rewards}': 'Rewards',
};

function sampleVars(env) {
  const now = Math.floor(env.now / 1000);
  const ago = (days) => now - days * 86400;
  const user = `<@${SAMPLE.user}>`;
  const count = env.memberCount;
  const ordinal = (n) => {
    const teen = n % 100 >= 11 && n % 100 <= 13;
    return `${fmt(n)}${teen ? 'th' : { 1: 'st', 2: 'nd', 3: 'rd' }[n % 10] ?? 'th'}`;
  };
  return {
    user, 'user.mention': user, 'user.name': 'SampleUser', 'user.username': 'SampleUser', 'user.idname': 'SampleUser',
    'user.tag': 'SampleUser', 'user.id': SAMPLE.user, 'user.avatar': SAMPLE.avatar, 'user.avatar_url': SAMPLE.avatar,
    'user.bot': 'False', 'user.display_name': 'SampleUser', 'user.nick': 'SampleUser', 'user.nickname': 'SampleUser',
    'user.created': `<t:${ago(1100)}:R>`, 'user.created_at': `<t:${ago(1100)}:D>`,
    'user.joined': `<t:${ago(60)}:R>`, 'user.joined_at': `<t:${ago(60)}:D>`,
    server: env.guildName, 'server.name': env.guildName, 'server.id': env.guildId, 'server.icon': env.guildIcon,
    'server.icon_url': env.guildIcon, 'server.owner': `<@${SAMPLE.owner}>`, 'server.owner_id': SAMPLE.owner,
    'server.member_count': fmt(count), 'server.member_count.ordinal': ordinal(count), 'server.verification_level': '2',
    'server.joined_at': `<t:${ago(400)}:D>`, 'server.created_at': `<t:${ago(1500)}:D>`,
    'server.boost_count': '7', 'server.boost_level': '2',
    channel: `<#${env.sampleChannel.id}>`, 'channel.mention': `<#${env.sampleChannel.id}>`,
    'channel.name': env.sampleChannel.name, 'channel.id': env.sampleChannel.id, 'channel.type': 'text',
    random: '7',
    // Feature variables (levels, achievements, tickets, economy, feeds, automations, ...).
    player: user, 'player.name': 'SampleUser', mention: user, level: '5', rank: '3', xp: '1,250',
    age: '25', count: fmt(count),
    role: `<@&${env.sampleRole.id}>`, 'role.id': env.sampleRole.id, 'role.name': env.sampleRole.name,
    number: '0042', reason: 'I need help with my order', topic: 'Support', panel: 'Support',
    managers: `<@&${env.sampleRole.id}>`, item: 'Golden Ticket', quantity: '1', coins: '500', price: '500',
    currency: 'coins', 'currency.emoji': '🪙', title: 'A sample post title', url: 'https://example.com/post',
    link: 'https://example.com/post', author: 'Sample Author', feed: 'Sample feed', index: '1', username: 'SampleUser',
    achievement: 'Chatterbox (Gold)', 'achievement.name': 'Chatterbox', 'achievement.description': 'Send messages',
    tier: 'Gold', 'tier.emoji': '🥇', goal: '1,000', progress: '1,000', rewards: '500 coins',
    automation: 'Sample automation', reaction: '👍', 'message.content': 'Hello there!', 'message.id': '100000000000000005',
    'message.author': user, 'message.author.name': 'SampleUser',
  };
}

const allowedGroups = (desc) => (Array.isArray(desc?.placeholderGroups) ? new Set(desc.placeholderGroups) : null);

function buildEnv(ctx, desc) {
  const res = isObj(ctx?.resources) ? ctx.resources : null;
  const channels = new Map();
  const roles = new Map();
  for (const ch of Array.isArray(res?.channels) ? res.channels : []) if (ch?.id) channels.set(ch.id, ch);
  for (const r of Array.isArray(res?.roles) ? res.roles : []) if (r?.id) roles.set(r.id, r);
  const sampleChannel = [...channels.values()].find((ch) => ch.kind === 'text')
    ?? { id: SAMPLE.channel, name: 'general', kind: 'text' };
  if (!channels.has(sampleChannel.id)) channels.set(sampleChannel.id, sampleChannel);
  const sampleRole = [...roles.values()].find((r) => !r.everyone && !r.managed)
    ?? { id: SAMPLE.role, name: 'Members', color: null };
  if (!roles.has(sampleRole.id)) roles.set(sampleRole.id, sampleRole);
  const guild = isObj(ctx?.guild) ? ctx.guild : {};
  const env = {
    channels,
    roles,
    users: new Map([[SAMPLE.user, 'SampleUser'], [SAMPLE.owner, 'ServerOwner']]),
    sampleChannel,
    sampleRole,
    now: Date.now(),
    guildName: str(guild.name) || 'My Server',
    guildId: str(guild.id) || str(ctx?.guildId) || '100000000000000000',
    guildIcon: typeof guild.iconUrl === 'string' ? guild.iconUrl : '',
    memberCount: Number.isInteger(guild.memberCount) && guild.memberCount > 0 ? guild.memberCount : 1234,
    defaultColor: isInt24(ctx?.defaultEmbedColor) ? ctx.defaultEmbedColor : DEFAULT_EMBED_COLOR,
    botName: str(ctx?.botName) || 'Bot',
    args: Array.isArray(desc?.placeholders) && desc.placeholders.some((t) => /^\{\d/.test(String(t))),
  };
  env.vars = sampleVars(env);
  // A message without a member (reminders, ...) may list the built-in groups it supports; the others stay literal.
  const groups = allowedGroups(desc);
  if (groups) {
    for (const key of Object.keys(env.vars)) {
      const group = /^(user|server|channel)(?:\.|$)/.exec(key)?.[1];
      if (group && !groups.has(group)) delete env.vars[key];
    }
  }
  return env;
}

// Replaces placeholders with sample values; unknown tokens stay literal (as the bot leaves them).
function substitute(text, env) {
  return String(text ?? '').replace(TOKEN_RE, (whole, inner) => {
    const raw = inner.trim();
    const name = raw.toLowerCase();
    if (Object.hasOwn(env.vars, name)) return env.vars[name];
    const rnd = /^random:(-?\d{1,10}):(-?\d{1,10})$/.exec(name);
    if (rnd) {
      const [x, y] = [Number(rnd[1]), Number(rnd[2])].sort((a, b) => a - b);
      return String(Math.floor((x + y) / 2));
    }
    if (env.args) {
      const arg = /^(\d{1,3})(?::([\s\S]*))?$/.exec(raw);
      if (arg) return arg[2] ? arg[2] : `word${arg[1]}`;
      if (/^(?:\d{1,3})?\.\.\.(?:\d{1,3})?$/.test(raw)) return 'some words';
    }
    return whole;
  });
}

/**
 * The text of a template part with placeholders replaced by sample values (no markdown), for list summaries.
 * @param {string} text
 * @param {object} [ctx]
 * @returns {string}
 */
export function sampleText(text, ctx = {}) {
  const env = buildEnv(ctx, null);
  return substitute(text, env)
    .replace(/<@!?(\d+)>/g, (_, id) => `@${env.users.get(id) ?? 'unknown-user'}`)
    .replace(/<#(\d+)>/g, (_, id) => `#${env.channels.get(id)?.name ?? 'unknown'}`)
    .replace(/<@&(\d+)>/g, (_, id) => `@${env.roles.get(id)?.name ?? 'deleted-role'}`)
    .replace(/<t:(-?\d{1,13})(?::([tTdDfFR]))?>/g, (_, s, style) => formatStamp(Number(s), style, env.now))
    .replace(/<a?:(\w{2,32}):\d{17,20}>/g, ':$1:');
}

/* ------------------------------------------------------------------ */
/* Markdown (DOM nodes only)                                            */
/* ------------------------------------------------------------------ */

const STAMP_FORMATS = {
  t: { hour: 'numeric', minute: '2-digit' },
  T: { hour: 'numeric', minute: '2-digit', second: '2-digit' },
  d: { day: '2-digit', month: '2-digit', year: 'numeric' },
  D: { day: 'numeric', month: 'long', year: 'numeric' },
  f: { day: 'numeric', month: 'long', year: 'numeric', hour: 'numeric', minute: '2-digit' },
  F: { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric', hour: 'numeric', minute: '2-digit' },
};

function formatStamp(sec, style = 'f', now = Date.now()) {
  const d = new Date(sec * 1000);
  if (!Number.isFinite(d.getTime())) return 'Invalid Date';
  try {
    if (style === 'R') {
      const diff = (d.getTime() - now) / 1000;
      const units = [['year', 31536000], ['month', 2592000], ['day', 86400], ['hour', 3600], ['minute', 60], ['second', 1]];
      const [unit, size] = units.find(([, s]) => Math.abs(diff) >= s) ?? units[units.length - 1];
      return new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' }).format(Math.round(diff / size), unit);
    }
    return new Intl.DateTimeFormat(undefined, STAMP_FORMATS[style] ?? STAMP_FORMATS.f).format(d);
  } catch {
    return d.toISOString();
  }
}

function clockNow() {
  try {
    return new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' }).format(new Date());
  } catch {
    return '12:00';
  }
}

// Inline rules, tried in order at a position (sticky regexes). Shapes follow Discord's (simple-markdown) rules.
const INLINE_RULES = [
  { type: 'escape', re: /\\([^A-Za-z0-9\s])/y },
  { type: 'code', re: /(`+)([^`][\s\S]*?)\1(?!`)/y },
  { type: 'link', re: /\[((?:\\[\s\S]|[^\\[\]\n]){1,256})\]\(\s*<?(https?:\/\/[^\s<>()]+(?:\([^\s<>()]*\)[^\s<>()]*)*)>?\s*\)/y },
  { type: 'autolink', re: /<(https?:\/\/[^\s>]+)>/y },
  { type: 'url', re: /https?:\/\/[^\s<]+[^\s<.,:;"'\])!?*_~|]/y, wordStart: true },
  { type: 'user', re: /<@!?(\d{1,20})>/y },
  { type: 'role', re: /<@&(\d{1,20})>/y },
  { type: 'channel', re: /<#(\d{1,20})>/y },
  { type: 'command', re: /<\/([^:<>\n]{1,96}):(\d{1,20})>/y },
  { type: 'time', re: /<t:(-?\d{1,13})(?::([tTdDfFR]))?>/y },
  { type: 'emoji', re: /<(a?):(\w{2,32}):(\d{17,20})>/y },
  { type: 'everyone', re: /@(everyone|here)(?![A-Za-z0-9_])/y },
  { type: 'strong', re: /\*\*((?:\\[\s\S]|[^\\])+?)\*\*(?!\*)/y },
  { type: 'u', re: /__((?:\\[\s\S]|[^\\])+?)__(?!_)/y },
  { type: 'em', re: /\*(?=\S)((?:\*\*|\\[\s\S]|\s+(?:\\[\s\S]|[^\s*\\]|\*\*)|[^\s*\\])+?)\*(?!\*)/y },
  { type: 'em', re: /_((?:__|\\[\s\S]|[^\\_])+?)_(?![A-Za-z0-9_])/y, wordStart: true },
  { type: 's', re: /~~([\s\S]+?)~~/y },
  { type: 'spoiler', re: /\|\|([\s\S]+?)\|\|/y },
];
const RULE_STARTS = '\\`[<h@*_~|';
const NEXT_SPECIAL = /[\\`[<h@*_~|]/g;
const MAX_DEPTH = 10;

function mention(text, extraClass = '') {
  return h('span', { class: `dp-mention${extraClass}` }, text);
}

function inlineNode(type, m, env, depth) {
  switch (type) {
    case 'escape': return document.createTextNode(m[1]);
    case 'code': return h('code', { class: 'dp-code' }, m[2]);
    case 'link': return h('span', { class: 'dp-link', title: m[2] }, renderInline(m[1], env, depth + 1));
    case 'autolink':
    case 'url': return h('span', { class: 'dp-link', title: m[1] ?? m[0] }, m[1] ?? m[0]);
    case 'user': return mention(`@${env.users.get(m[1]) ?? 'unknown-user'}`);
    case 'role': {
      const role = env.roles.get(m[1]);
      const el = mention(`@${role?.name ?? 'deleted-role'}`, ' is-role');
      if (role?.color) el.style.setProperty('--role-color', role.color);
      return el;
    }
    case 'channel': {
      const ch = env.channels.get(m[1]);
      const voice = ch && (ch.kind === 'voice' || ch.kind === 'stage');
      return mention(`${voice ? '🔊' : '#'}${ch?.name ?? 'unknown'}`);
    }
    case 'command': return mention(`/${m[1]}`);
    case 'time': return h('span', { class: 'dp-stamp-pill' }, formatStamp(Number(m[1]), m[2] || 'f', env.now));
    case 'emoji': {
      const img = h('img', {
        class: 'dp-emoji',
        src: `https://cdn.discordapp.com/emojis/${m[3]}.${m[1] ? 'gif' : 'webp'}?size=48`,
        alt: `:${m[2]}:`,
        title: `:${m[2]}:`,
        loading: 'lazy',
        referrerpolicy: 'no-referrer',
      });
      img.addEventListener('error', () => img.replaceWith(document.createTextNode(`:${m[2]}:`)), { once: true });
      return img;
    }
    case 'everyone': return mention(`@${m[1]}`);
    case 'strong': return h('strong', {}, renderInline(m[1], env, depth + 1));
    case 'u': return h('u', {}, renderInline(m[1], env, depth + 1));
    case 'em': return h('em', {}, renderInline(m[1], env, depth + 1));
    case 's': return h('s', {}, renderInline(m[1], env, depth + 1));
    case 'spoiler': {
      const el = h('span', { class: 'dp-spoiler', role: 'button', tabindex: '0', 'aria-pressed': 'false', title: 'Spoiler' },
        renderInline(m[1], env, depth + 1));
      const toggle = () => {
        const shown = el.classList.toggle('is-shown');
        el.setAttribute('aria-pressed', shown ? 'true' : 'false');
      };
      el.addEventListener('click', toggle);
      el.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          toggle();
        }
      });
      return el;
    }
    default: return null;
  }
}

function renderInline(src, env, depth = 0) {
  const out = [];
  let buf = '';
  let i = 0;
  const flush = () => {
    if (buf) out.push(document.createTextNode(buf));
    buf = '';
  };
  outer: while (i < src.length) {
    if (depth < MAX_DEPTH && RULE_STARTS.includes(src[i])) {
      for (const rule of INLINE_RULES) {
        if (rule.wordStart && i > 0 && /[A-Za-z0-9_]/.test(src[i - 1])) continue;
        rule.re.lastIndex = i;
        const m = rule.re.exec(src);
        if (!m) continue;
        const node = inlineNode(rule.type, m, env, depth);
        if (!node) continue;
        flush();
        out.push(node);
        i += m[0].length;
        continue outer;
      }
    }
    NEXT_SPECIAL.lastIndex = i + 1;
    const next = NEXT_SPECIAL.exec(src);
    const end = next ? next.index : src.length;
    buf += src.slice(i, end);
    i = end;
  }
  flush();
  return out;
}

// Line-level markdown: quotes, headings, subtext, lists; other lines keep their line breaks.
function renderLines(text, env, inQuote = false) {
  const nodes = [];
  const lines = text.split('\n');
  let para = [];
  let list = null;
  let quote = null;
  const flushPara = () => {
    // A trailing empty line would collapse at the end of a block; Discord shows it, so keep it visible.
    const text = `${para.join('\n')}${para[para.length - 1] === '' ? '\u200b' : ''}`;
    if (para.length) nodes.push(h('div', { class: 'dp-p' }, renderInline(text, env)));
    para = [];
  };
  const flushQuote = () => {
    if (quote) nodes.push(h('div', { class: 'dp-quote' }, renderLines(quote.join('\n'), env, true)));
    quote = null;
  };
  for (let idx = 0; idx < lines.length; idx += 1) {
    const line = lines[idx];
    if (!inQuote) {
      if (line === '>>>' || line.startsWith('>>> ')) {
        flushPara();
        flushQuote();
        list = null;
        quote = [line.slice(4), ...lines.slice(idx + 1)];
        break;
      }
      if (line === '>' || line.startsWith('> ')) {
        flushPara();
        list = null;
        (quote ??= []).push(line.slice(2));
        continue;
      }
      flushQuote();
    }
    const head = /^(#{1,3}) +(\S.*)$/.exec(line);
    if (head) {
      flushPara();
      list = null;
      nodes.push(h('div', { class: `dp-h dp-h${head[1].length}` }, renderInline(head[2].trim(), env)));
      continue;
    }
    const sub = /^-# +(\S.*)$/.exec(line);
    if (sub) {
      flushPara();
      list = null;
      nodes.push(h('div', { class: 'dp-subtext' }, renderInline(sub[1], env)));
      continue;
    }
    const li = /^( *)(?:[-*]|(\d{1,9})\.) +(\S.*)$/.exec(line);
    if (li) {
      flushPara();
      const tag = li[2] === undefined ? 'ul' : 'ol';
      if (!list || list.tag !== tag) {
        list = { tag, el: h(tag, { class: 'dp-list', start: tag === 'ol' && li[2] !== '1' ? li[2] : null }) };
        nodes.push(list.el);
      }
      list.el.append(h('li', { class: li[1].length >= 2 ? 'is-nested' : null }, renderInline(li[3], env)));
      continue;
    }
    list = null;
    para.push(line);
  }
  flushPara();
  flushQuote();
  return nodes;
}

// Full markdown: code blocks first, the rest line by line.
function renderBlocks(text, env) {
  const nodes = [];
  const re = /```(?:([A-Za-z0-9_+.#-]{1,24})\n)?([\s\S]*?)```/g;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    let before = text.slice(last, m.index);
    if (before.endsWith('\n')) before = before.slice(0, -1);
    if (before) nodes.push(...renderLines(before, env));
    nodes.push(h('pre', { class: 'dp-pre' }, h('code', {}, m[2].replace(/^\n/, '').replace(/\n$/, ''))));
    last = re.lastIndex;
    if (text[last] === '\n') last += 1;
  }
  const rest = text.slice(last);
  if (rest) nodes.push(...renderLines(rest, env));
  return nodes;
}

/* ------------------------------------------------------------------ */
/* Preview                                                              */
/* ------------------------------------------------------------------ */

function imageBox(kind, label) {
  const small = kind === 'author' || kind === 'footer' || kind === 'emoji';
  return h(
    'span',
    { class: `dp-img-ph dp-img-${kind}`, role: 'img', 'aria-label': `Image from ${label}`, title: label },
    small ? null : [uiIcon('image'), h('span', { class: 'dp-img-host' }, label)],
  );
}

// An image of the preview. Only https images from hosts the dashboard's CSP allows are loaded; any other host
// shows a neutral box with the host name. A link that is not http(s) is left out, like the bot leaves it out.
function previewImage(raw, env, kind) {
  const url = substitute(raw, env).trim();
  if (!url) return null;
  let parsed = null;
  try {
    parsed = new URL(url);
  } catch {
    parsed = null;
  }
  if (!parsed || !/^https?:$/.test(parsed.protocol) || !parsed.hostname || /\s/.test(url)) {
    if (ATTACHMENT_RE.test(url)) return imageBox(kind, 'attachment');
    if (url.includes('{')) return imageBox(kind, url);
    return null;
  }
  if (parsed.protocol === 'https:' && IMAGE_HOSTS.has(parsed.hostname)) {
    const img = h('img', {
      class: `dp-img dp-img-${kind}`,
      src: parsed.href,
      alt: '',
      loading: 'lazy',
      decoding: 'async',
      referrerpolicy: 'no-referrer',
    });
    img.addEventListener('error', () => img.replaceWith(imageBox(kind, parsed.hostname)), { once: true });
    return img;
  }
  return imageBox(kind, parsed.hostname);
}

const embedVisible = (e) => Boolean(e && (
  str(e.title).trim() || str(e.description).trim() || (Array.isArray(e.fields) && e.fields.length)
  || str(e.author?.name).trim() || str(e.footer?.text).trim() || str(e.image).trim() || str(e.thumbnail).trim()
));

// Discord lays inline fields out three per row (two next to a thumbnail); a non-inline field takes a whole row.
function fieldWidths(fields, perRow) {
  const widths = [];
  let row = [];
  const flush = () => {
    for (const idx of row) widths[idx] = 12 / row.length;
    row = [];
  };
  fields.forEach((f, idx) => {
    if (!f.inline) {
      flush();
      widths[idx] = 12;
      return;
    }
    row.push(idx);
    if (row.length === perRow) flush();
  });
  flush();
  return widths;
}

function buildEmbed(e, env) {
  const thumb = e.thumbnail ? previewImage(e.thumbnail, env, 'thumb') : null;
  const grid = h('div', { class: `dp-embed-grid${thumb ? ' has-thumb' : ''}` });
  const authorName = substitute(e.author?.name, env).trim();
  if (authorName) {
    const iconEl = e.author.iconUrl ? previewImage(e.author.iconUrl, env, 'author') : null;
    const authorUrl = substitute(e.author.url, env).trim();
    const linked = isHttpUrl(authorUrl);
    grid.append(h('div', { class: 'dp-author' }, iconEl,
      h('span', { class: `dp-author-name${linked ? ' is-link' : ''}`, title: linked ? authorUrl : null },
        authorName.slice(0, L.author))));
  }
  const title = substitute(e.title, env).trim();
  if (title) {
    const url = substitute(e.url, env).trim();
    grid.append(h('div', { class: `dp-title${isHttpUrl(url) ? ' is-link' : ''}`, title: isHttpUrl(url) ? url : null },
      renderInline(title.slice(0, L.title), env)));
  }
  const description = substitute(e.description, env);
  if (description.trim()) grid.append(h('div', { class: 'dp-desc' }, renderBlocks(description.slice(0, L.description), env)));
  const fields = Array.isArray(e.fields) ? e.fields.slice(0, L.fields) : [];
  if (fields.length) {
    const widths = fieldWidths(fields, thumb ? 2 : 3);
    grid.append(h('div', { class: 'dp-fields' }, fields.map((f, idx) => {
      const name = substitute(f.name, env).trim();
      const value = substitute(f.value, env);
      const complete = name !== '' && value.trim() !== '';
      return h(
        'div',
        {
          class: `dp-field w-${widths[idx]}${complete ? '' : ' is-incomplete'}`,
          title: complete ? null : 'Left out until it has a name and a value',
        },
        h('div', { class: 'dp-field-name' }, name ? renderInline(name.slice(0, L.fieldName), env) : '​'),
        h('div', { class: 'dp-field-value' }, value.trim() ? renderBlocks(value.slice(0, L.fieldValue), env) : '​'),
      );
    })));
  }
  if (thumb) grid.append(h('div', { class: 'dp-thumb' }, thumb));
  const image = e.image ? previewImage(e.image, env, 'image') : null;
  if (image) grid.append(h('div', { class: 'dp-image' }, image));
  const footerText = substitute(e.footer?.text, env).trim();
  if (footerText || e.timestamp) {
    const iconEl = footerText && e.footer?.iconUrl ? previewImage(e.footer.iconUrl, env, 'footer') : null;
    const parts = [footerText.slice(0, L.footer), e.timestamp ? `Today at ${clockNow()}` : ''].filter(Boolean);
    grid.append(h('div', { class: 'dp-footer' }, iconEl, h('span', {}, parts.join(' • '))));
  }
  const el = h('div', { class: 'dp-embed' }, grid);
  el.style.setProperty('--embed-color', colorHex(Number.isInteger(e.color) ? e.color : env.defaultColor));
  return el;
}

function buildButton(b, env) {
  const label = substitute(b.label, env).trim().slice(0, L.buttonLabel);
  const url = substitute(b.url, env).trim();
  const ok = label !== '' && isHttpUrl(url) && url.length <= L.buttonUrl;
  let emoji = null;
  const e = str(b.emoji).trim();
  if (e) {
    const custom = CUSTOM_EMOJI_RE.exec(e);
    if (custom) {
      emoji = h('img', {
        class: 'dp-emoji', alt: `:${custom[2]}:`, referrerpolicy: 'no-referrer',
        src: `https://cdn.discordapp.com/emojis/${custom[3]}.${custom[1] ? 'gif' : 'webp'}?size=48`,
      });
      emoji.addEventListener('error', () => emoji.replaceWith(imageBox('emoji', custom[2])), { once: true });
    } else if (isButtonEmoji(e)) {
      emoji = h('span', { class: 'dp-btn-emoji' }, e);
    }
  }
  return h(
    'span',
    { class: `dp-btn${ok ? '' : ' is-incomplete'}`, title: ok ? url : 'Left out until it has a label and an http(s) link' },
    emoji,
    label || 'Button',
    uiIcon('external'),
  );
}

function buildMessage(tpl, env) {
  const content = str(tpl?.content);
  const embed = isObj(tpl?.embed) && embedVisible(tpl.embed) ? buildEmbed(tpl.embed, env) : null;
  const buttons = (Array.isArray(tpl?.buttons) ? tpl.buttons : [])
    .filter((b) => isObj(b) && (str(b.label).trim() || str(b.url).trim()));
  const rows = [];
  for (let i = 0; i < Math.min(buttons.length, BUTTONS_PER_ROW * MAX_BUTTON_ROWS); i += BUTTONS_PER_ROW) {
    rows.push(h('div', { class: 'dp-row' }, buttons.slice(i, i + BUTTONS_PER_ROW).map((b) => buildButton(b, env))));
  }
  const empty = !content.trim() && !embed && !rows.length;
  return h(
    'div',
    { class: 'dp-msg' },
    h('div', { class: 'dp-avatar' }, h('img', { src: '/favicon.svg', alt: '' })),
    h(
      'div',
      { class: 'dp-main' },
      h('div', { class: 'dp-header' },
        h('span', { class: 'dp-name' }, env.botName),
        h('span', { class: 'dp-tag' }, 'BOT'),
        h('span', { class: 'dp-time' }, `Today at ${clockNow()}`)),
      content.trim() ? h('div', { class: 'dp-content' }, renderBlocks(substitute(content, env).slice(0, L.content + 200), env))
        : null,
      embed,
      rows.length ? h('div', { class: 'dp-buttons' }, rows) : null,
      empty ? h('div', { class: 'dp-empty' }, 'Nothing to show yet. Add some text or an embed.') : null,
    ),
  );
}

/**
 * A Discord-style preview of a message template ({ content, embed, buttons }; a bare EmbedSpec is not accepted).
 * Placeholders show sample values ({user} -> @SampleUser, {server} -> the server name, ...); only images from
 * cdn.discordapp.com are loaded. Follows the page's light/dark theme.
 * @param {{ content?: string|null, embed?: object|null, buttons?: object[] }} template
 * @param {object} [ctx] the page context (guild, resources, ...; see the dashboard contract)
 * @returns {HTMLElement}
 */
export function renderMessagePreview(template, ctx = {}) {
  const root = h('div', { class: 'dp', role: 'group', 'aria-label': 'Message preview' });
  root.append(buildMessage(isObj(template) ? template : {}, buildEnv(ctx, null)));
  return root;
}

/**
 * A one-line summary of a template for lists: its colour, a title and whether it has an embed.
 * @param {unknown} template
 * @param {object} [ctx]
 * @returns {{ color: string|null, title: string, hasEmbed: boolean, buttons: number }}
 */
export function templateSummary(template, ctx = {}) {
  const t = isObj(template) ? template : {};
  const e = isObj(t.embed) ? t.embed : null;
  const firstLine = (s) => sampleText(str(s), ctx).split('\n').map((x) => x.replace(/[*_~`|>#]/g, '').trim()).find(Boolean) ?? '';
  const title = (e && (firstLine(e.title) || firstLine(e.author?.name) || firstLine(e.description))) || firstLine(t.content);
  const fallback = isInt24(ctx?.defaultEmbedColor) ? ctx.defaultEmbedColor : DEFAULT_EMBED_COLOR;
  return {
    color: e ? colorHex(Number.isInteger(e.color) ? e.color : fallback) : null,
    title,
    hasEmbed: Boolean(e && embedVisible(e)),
    buttons: Array.isArray(t.buttons) ? t.buttons.length : 0,
  };
}

/* ------------------------------------------------------------------ */
/* Editor state <-> wire shape                                          */
/* ------------------------------------------------------------------ */

function embedState(e) {
  const a = isObj(e.author) ? e.author : {};
  const f = isObj(e.footer) ? e.footer : {};
  const color = Number.isInteger(e.color) ? e.color : typeof e.color === 'string' ? parseHex(e.color) : null;
  return {
    title: str(e.title),
    description: str(e.description),
    url: str(e.url),
    color: isInt24(color) ? color : null,
    author: { name: str(a.name), iconUrl: str(a.iconUrl ?? a.icon_url), url: str(a.url) },
    footer: { text: str(f.text), iconUrl: str(f.iconUrl ?? f.icon_url) },
    thumbnail: str(isObj(e.thumbnail) ? e.thumbnail.url : e.thumbnail),
    image: str(isObj(e.image) ? e.image.url : e.image),
    fields: (Array.isArray(e.fields) ? e.fields : []).filter(isObj)
      .map((x) => ({ uid: nextUid(), name: str(x.name), value: str(x.value), inline: x.inline === true })),
    timestamp: e.timestamp === true || (typeof e.timestamp === 'string' && e.timestamp !== ''),
  };
}

function toState(value, embedOnly) {
  if (embedOnly) return { content: '', embed: embedState(isObj(value) ? value : {}), buttons: [] };
  const t = isObj(value) ? value : {};
  return {
    content: str(t.content),
    embed: isObj(t.embed) ? embedState(t.embed) : null,
    buttons: (Array.isArray(t.buttons) ? t.buttons : []).filter(isObj)
      .map((b) => ({ uid: nextUid(), label: str(b.label), url: str(b.url), emoji: str(b.emoji) })),
  };
}

const orNull = (s) => (s.trim() ? s : null);
const urlOrNull = (s) => (s.trim() ? s.trim() : null);

function embedOut(e) {
  const author = e.author.name.trim() || e.author.iconUrl.trim() || e.author.url.trim()
    ? { name: e.author.name, iconUrl: urlOrNull(e.author.iconUrl), url: urlOrNull(e.author.url) }
    : null;
  const footer = e.footer.text.trim() || e.footer.iconUrl.trim()
    ? { text: e.footer.text, iconUrl: urlOrNull(e.footer.iconUrl) }
    : null;
  return {
    title: orNull(e.title),
    description: orNull(e.description),
    url: urlOrNull(e.url),
    color: e.color,
    author,
    footer,
    thumbnail: urlOrNull(e.thumbnail),
    image: urlOrNull(e.image),
    fields: e.fields.map((f) => ({ name: f.name, value: f.value, inline: f.inline })),
    timestamp: e.timestamp,
  };
}

function templateOut(state) {
  return {
    content: orNull(state.content),
    embed: state.embed ? embedOut(state.embed) : null,
    buttons: state.buttons.map((b) => ({ label: b.label.trim(), url: b.url.trim(), emoji: b.emoji.trim() || null })),
  };
}

const embedTextLength = (e) => e.title.length + e.description.length + e.author.name.length + e.footer.text.length
  + e.fields.reduce((n, f) => n + f.name.length + f.value.length, 0);

/* ------------------------------------------------------------------ */
/* Validation (mirrors validateTemplate)                                */
/* ------------------------------------------------------------------ */

function urlProblem(value, { label, imageLike = false, max = L.url }) {
  const v = value.trim();
  if (!v) return null;
  if (v.length > max) return `${label} is longer than ${fmt(max)} characters.`;
  if (isHttpUrl(v) || v.includes('{') || (imageLike && ATTACHMENT_RE.test(v))) return null;
  return `${label} must be an http(s) link, or a variable such as {user.avatar}.`;
}

/**
 * Problems of the editor state: `error` = the server would refuse the save, `warn` = that part is left out.
 * @returns {{ path: string, message: string, level: 'error'|'warn' }[]}
 */
function validateState(state, { embedOnly, requireContent, touched }) {
  const issues = [];
  const add = (path, message, level = 'error') => issues.push({ path, message, level });
  const tooLong = (path, what, len, max) => {
    if (len > max) add(path, `${what} is ${fmt(len)} characters (max ${fmt(max)}).`);
  };
  if (!embedOnly) tooLong('content', 'Message content', state.content.length, L.content);
  const e = state.embed;
  if (e) {
    tooLong('embed.author.name', 'Author name', e.author.name.length, L.author);
    tooLong('embed.title', 'Title', e.title.length, L.title);
    tooLong('embed.description', 'Description', e.description.length, L.description);
    tooLong('embed.footer.text', 'Footer', e.footer.text.length, L.footer);
    if (e.fields.length > L.fields) add('embed.fields', `The embed has ${e.fields.length} fields (max ${L.fields}).`);
    e.fields.forEach((f, idx) => {
      const n = idx + 1;
      tooLong(`embed.fields.${idx}.name`, `Field ${n} name`, f.name.length, L.fieldName);
      tooLong(`embed.fields.${idx}.value`, `Field ${n} value`, f.value.length, L.fieldValue);
      const hasName = f.name.trim() !== '';
      const hasValue = f.value.trim() !== '';
      if (!hasName || !hasValue) {
        const where = `embed.fields.${idx}.${hasName ? 'value' : 'name'}`;
        add(where, `Field ${n} needs a name and a value, or it is left out.`, 'warn');
      }
    });
    const total = embedTextLength(e);
    if (total > L.total) add('embed.total', `The embed's text totals ${fmt(total)} characters (max ${fmt(L.total)}).`);
    for (const [path, label, value, imageLike] of [
      ['embed.url', 'The title link', e.url, false],
      ['embed.author.url', 'The author link', e.author.url, false],
      ['embed.author.iconUrl', 'The author icon', e.author.iconUrl, true],
      ['embed.footer.iconUrl', 'The footer icon', e.footer.iconUrl, true],
      ['embed.thumbnail', 'The thumbnail', e.thumbnail, true],
      ['embed.image', 'The image', e.image, true],
    ]) {
      const problem = urlProblem(value, { label, imageLike });
      if (problem) add(path, problem);
    }
    if (!e.author.name.trim() && (e.author.iconUrl.trim() || e.author.url.trim())) {
      add('embed.author.name', 'Add an author name, or the author line is left out.', 'warn');
    }
    if (!e.footer.text.trim() && e.footer.iconUrl.trim()) {
      add('embed.footer.text', 'Add footer text, or the footer icon is left out.', 'warn');
    }
  }
  if (!embedOnly) {
    if (state.buttons.length > L.buttons) add('buttons', `Too many link buttons (${state.buttons.length}, max ${L.buttons}).`);
    state.buttons.forEach((b, idx) => {
      const n = idx + 1;
      const label = b.label.trim();
      const url = b.url.trim();
      if (!label || !url) {
        add(`buttons.${idx}.${label ? 'url' : 'label'}`, `Button ${n} needs a label and a link, or it is left out.`, 'warn');
      }
      if (label.length > L.buttonLabel) {
        add(`buttons.${idx}.label`, `Button ${n} label is ${label.length} characters (max ${L.buttonLabel}).`);
      }
      const problem = urlProblem(b.url, { label: `Button ${n} link`, max: L.buttonUrl });
      if (problem) add(`buttons.${idx}.url`, problem);
      if (b.emoji.trim() && !isButtonEmoji(b.emoji)) {
        add(`buttons.${idx}.emoji`, `Button ${n} emoji must be one emoji, like 🎉 or <:name:id>.`);
      }
    });
    const complete = state.buttons.some((b) => b.label.trim() && b.url.trim());
    const visible = state.embed ? embedVisible(embedOut(state.embed)) : false;
    if (requireContent && touched && !state.content.trim() && !visible && !complete) {
      add('', EMPTY_MESSAGE);
    }
  }
  return issues;
}

// Routes a server message (customEmbed's wording) to the part it is about.
function guessPath(message) {
  const m = String(message ?? '');
  let r;
  if ((r = /Field (\d+) (name|value)/i.exec(m))) return `embed.fields.${Number(r[1]) - 1}.${r[2].toLowerCase()}`;
  if ((r = /Button (\d+) (label|URL|emoji)/i.exec(m))) return `buttons.${Number(r[1]) - 1}.${r[2].toLowerCase()}`;
  const table = [
    [/Message content/i, 'content'], [/Author name/i, 'embed.author.name'], [/Author URL/i, 'embed.author.url'],
    [/Author icon/i, 'embed.author.iconUrl'], [/Footer icon/i, 'embed.footer.iconUrl'], [/Footer is/i, 'embed.footer.text'],
    [/Embed URL/i, 'embed.url'], [/Thumbnail/i, 'embed.thumbnail'], [/Image must|Image is/i, 'embed.image'],
    [/Description is/i, 'embed.description'], [/Title is/i, 'embed.title'], [/Embed has \d+ fields/i, 'embed.fields'],
    [/Embed text totals/i, 'embed.total'], [/link buttons/i, 'buttons'], [/Colour/i, 'embed.color'],
  ];
  for (const [re, path] of table) if (re.test(m)) return path;
  return '';
}

/* ------------------------------------------------------------------ */
/* The editor                                                           */
/* ------------------------------------------------------------------ */

function placeholderEntries(ctx, desc) {
  const seen = new Set();
  const out = [];
  for (const token of Array.isArray(desc?.placeholders) ? desc.placeholders : []) {
    if (typeof token !== 'string' || !token || seen.has(token)) continue;
    seen.add(token);
    out.push({ token, description: FEATURE_DESCRIPTIONS[token] ?? 'Supported by this message', group: 'feature' });
  }
  const builtins = Array.isArray(ctx?.placeholders) && ctx.placeholders.length ? ctx.placeholders : FALLBACK_PLACEHOLDERS;
  const groups = allowedGroups(desc);
  for (const p of builtins) {
    if (!isObj(p) || typeof p.token !== 'string' || seen.has(p.token)) continue;
    const group = ['user', 'server', 'channel'].includes(p.group) ? p.group : 'other';
    if (groups && !groups.has(group)) continue;
    seen.add(p.token);
    out.push({ token: p.token, description: str(p.description), group });
  }
  return out;
}

const GROUP_LABELS = { feature: 'This message', user: 'Member', server: 'Server', channel: 'Channel', other: 'Other' };

let fieldSizing; // whether textareas can size themselves (CSS field-sizing)
function autoGrow(ta) {
  if (fieldSizing === undefined) {
    fieldSizing = typeof CSS !== 'undefined' && typeof CSS.supports === 'function' && CSS.supports('field-sizing', 'content');
  }
  if (fieldSizing || !ta.isConnected) return;
  ta.style.setProperty('height', 'auto');
  ta.style.setProperty('height', `${ta.scrollHeight + 2}px`);
}

/**
 * The message editor: a FieldControl (see the dashboard contract, section 8) for a `template` descriptor
 * ({ content, embed, buttons }) or an `embed` descriptor (an EmbedSpec, or null when the descriptor is nullable and
 * the embed is empty). Two columns (editor, live preview) when there is room, otherwise Edit / Preview tabs.
 * Optional descriptor keys read here: `placeholders` (extra tokens offered as chips), `requireContent`, `nullable`
 * (embed), `placeholderGroups` (e.g. ['server', 'channel', 'other'] for a message without a member: the built-in
 * tokens of other groups are not offered and stay literal in the preview). `ctx.defaultEmbedColor` (an integer)
 * replaces #5865F2 as the colour shown for an embed without one.
 * @param {object} desc the public descriptor
 * @param {{ value?: unknown, ctx?: object, onChange?: (value: unknown) => void, idPrefix?: string, disabled?: boolean,
 *           showLabel?: boolean }} [opts] showLabel false hides the legend visually (when the caller labels it)
 * @returns {{ el: HTMLElement, getValue: () => unknown, setValue: (v: unknown) => void,
 *             setError: (message: string|null, subPath?: string) => void, setResources: (resources: object) => void,
 *             focus: () => void, destroy: () => void }}
 */
export function createTemplateEditor(desc, opts = {}) {
  const { value, ctx = {}, onChange, idPrefix = 'f', disabled = false, showLabel = true } = opts;
  const d = isObj(desc) ? desc : { type: 'template', label: 'Message' };
  const embedOnly = d.type === 'embed';
  const requireContent = !embedOnly && d.requireContent !== false;
  const base = `${idPrefix}-${String(d.field ?? 'message').replace(/[^A-Za-z0-9_-]/g, '-')}`;
  let localCtx = isObj(ctx) ? ctx : {};
  let env = buildEnv(localCtx, d);
  let state = toState(value, embedOnly);
  let touched = false;
  let serverError = null; // { message, path }
  const localErrors = new Map(); // path -> message (input that could not be parsed, e.g. a bad hex colour)
  let lastText = null; // the text box that variables are inserted into
  let removedEmbed = null; // for "Undo" after removing the embed
  let frame = 0;
  let destroyed = false;
  let controls = new Map(); // path -> { input, msg, counter, max, details }
  const blockMsgs = new Map(); // path -> message element for list-level problems

  const getValue = () => {
    if (!embedOnly) return templateOut(state);
    const out = embedOut(state.embed);
    return d.nullable && !embedVisible(out) && out.color === null ? null : out;
  };
  const previewTemplate = () => (embedOnly ? { content: null, embed: embedOut(state.embed), buttons: [] } : templateOut(state));

  /* ---- shell ---- */

  const legendId = `${base}-legend`;
  const descId = d.description ? `${base}-desc` : null;
  const root = h('fieldset', { class: 'te', id: base, 'aria-describedby': descId });
  const legend = h('legend', { class: `te-legend${showLabel ? '' : ' sr-only'}`, id: legendId },
    d.label || (embedOnly ? 'Embed' : 'Message'));
  const serverBox = h('div', { class: 'te-alert', role: 'alert', hidden: true });
  const editCol = h('div', { class: 'te-edit' });
  const previewHost = h('div', { class: 'dp', role: 'group', 'aria-label': 'Message preview' });
  const previewCol = h(
    'div',
    { class: 'te-preview', id: `${base}-preview` },
    h('div', { class: 'te-preview-head' },
      h('span', { class: 'te-preview-title' }, uiIcon('eye'), 'Preview'),
      h('span', { class: 'te-preview-note' }, 'Variables show sample values')),
    previewHost,
  );
  const layout = h('div', { class: 'te-layout', 'data-pane': 'edit' }, editCol, previewCol);
  const tab = (pane, iconName, text, on) => h(
    'button',
    { type: 'button', class: 'seg-btn', 'aria-pressed': on ? 'true' : 'false', 'aria-controls': `${base}-${pane}` },
    uiIcon(iconName),
    text,
  );
  const tabEdit = tab('edit', 'pencil', 'Edit', true);
  const tabPreview = tab('preview', 'eye', 'Preview', false);
  editCol.id = `${base}-edit`;
  const showPane = (pane) => {
    layout.dataset.pane = pane;
    tabEdit.setAttribute('aria-pressed', pane === 'edit' ? 'true' : 'false');
    tabPreview.setAttribute('aria-pressed', pane === 'preview' ? 'true' : 'false');
    if (pane === 'edit') for (const ta of editCol.querySelectorAll('textarea')) autoGrow(ta);
  };
  tabEdit.addEventListener('click', () => showPane('edit'));
  tabPreview.addEventListener('click', () => showPane('preview'));
  const tabs = h('div', { class: 'te-tabs segmented', role: 'group', 'aria-label': 'Show' }, tabEdit, tabPreview);
  root.append(
    legend,
    d.description ? h('p', { class: 'te-desc', id: descId }, d.description) : null,
    h('div', { class: 'te-wrap' }, serverBox, tabs, layout),
  );
  // Read-only mode disables the inputs but keeps the Edit / Preview tabs and the sections usable.
  const applyDisabled = () => {
    if (!disabled) return;
    for (const el of editCol.querySelectorAll('input, textarea, button, select')) el.disabled = true;
  };

  /* ---- change handling ---- */

  function schedulePreview() {
    if (destroyed) return;
    if (typeof requestAnimationFrame !== 'function') {
      renderPreview();
      return;
    }
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      renderPreview();
    });
  }

  function renderPreview() {
    if (destroyed) return;
    try {
      previewHost.replaceChildren(buildMessage(previewTemplate(), env));
    } catch {
      previewHost.replaceChildren(h('div', { class: 'dp-empty' }, 'The preview could not be drawn.'));
    }
  }

  function changed() {
    touched = true;
    serverError = null;
    paintIssues();
    schedulePreview();
    if (typeof onChange === 'function') onChange(getValue());
  }

  /* ---- controls ---- */

  function register(path, entry) {
    controls.set(path, entry);
    return entry;
  }

  // A labelled text box bound to `obj[key]`. `path` is where its problems are reported; `key` makes the id stable.
  function textBox(spec) {
    const { path, idKey, label, obj, key, max = 0, multiline = false } = spec;
    const { placeholder = '', url = false, hint = '', mono = false } = spec;
    const id = `${base}-${idKey}`;
    const msgId = `${id}-msg`;
    const hintId = hint ? `${id}-hint` : null;
    const counter = max ? h('span', { class: 'te-count', id: `${id}-count` }) : null;
    const attrs = {
      id,
      class: `te-input${multiline ? ' te-textarea' : ''}${mono || url ? ' is-mono' : ''}`,
      placeholder: placeholder || null,
      spellcheck: url ? 'false' : null,
      autocomplete: 'off',
      inputmode: url ? 'url' : null,
      rows: multiline ? '3' : null,
      'aria-describedby': [msgId, hintId, counter ? `${id}-count` : null].filter(Boolean).join(' '),
    };
    const input = multiline ? h('textarea', attrs) : h('input', { type: 'text', ...attrs });
    input.value = obj[key];
    const msg = h('p', { class: 'te-msg', id: msgId, hidden: true });
    const paintCount = () => {
      if (!counter) return;
      const n = input.value.length;
      counter.textContent = `${fmt(n)} / ${fmt(max)}`;
      counter.classList.toggle('is-near', n > max * 0.9 && n <= max);
      counter.classList.toggle('is-over', n > max);
    };
    paintCount();
    input.addEventListener('input', () => {
      obj[key] = input.value;
      paintCount();
      if (multiline) autoGrow(input);
      changed();
    });
    input.addEventListener('focus', () => {
      lastText = input;
      if (multiline) autoGrow(input);
    });
    const wrap = h(
      'div',
      { class: 'te-row' },
      h('div', { class: 'te-label-row' }, h('label', { class: 'te-label', for: id }, label), counter),
      input,
      hint ? h('p', { class: 'te-hint', id: hintId }, hint) : null,
      msg,
    );
    register(path, { input, msg, counter, max });
    return wrap;
  }

  function section(title, { open = true, badge = null, extra = null, cls = '' } = {}) {
    const body = h('div', { class: 'te-group-body' });
    const details = h('details', { class: `te-group${cls ? ` ${cls}` : ''}`, open },
      h('summary', { class: 'te-summary' }, h('span', { class: 'te-summary-title' }, title), badge, extra, icon('chevron')),
      body);
    return { details, body };
  }

  function switchRow(idKey, label, on, set) {
    const labelId = `${base}-${idKey}-label`;
    const sw = createSwitch({
      labelId,
      onToggle: () => {
        const next = sw.getAttribute('aria-checked') !== 'true';
        paintSwitch(sw, next, false);
        set(next);
        changed();
      },
    });
    paintSwitch(sw, on, false);
    return h('div', { class: 'te-switch-row' }, h('span', { class: 'te-switch-label', id: labelId }, label), sw);
  }

  function iconButton(name, label, onClick, { disabled: off = false, cls = '' } = {}) {
    return h('button', {
      type: 'button',
      class: `icon-btn te-icon-btn${cls ? ` ${cls}` : ''}`,
      'aria-label': label,
      title: label,
      disabled: off,
      onclick: onClick,
    }, uiIcon(name));
  }

  /* ---- variables ---- */

  function insertToken(token) {
    let target = lastText && lastText.isConnected && editCol.contains(lastText) ? lastText : null;
    if (!target) target = controls.get(embedOnly ? 'embed.description' : 'content')?.input ?? null;
    if (!target || target.disabled) return;
    const start = target.selectionStart ?? target.value.length;
    const end = target.selectionEnd ?? start;
    target.focus();
    target.setRangeText(token, start, end, 'end');
    target.dispatchEvent(new Event('input', { bubbles: true }));
  }

  function chip(entry, featured = false) {
    const btn = h('button', {
      type: 'button',
      class: `te-chip${featured && entry.group === 'feature' ? ' is-feature' : ''}`,
      title: entry.description ? `${entry.description}. Click to insert.` : 'Click to insert.',
    }, entry.token);
    // Keep the caret in the text box: a mouse press must not move focus to the chip.
    btn.addEventListener('mousedown', (e) => e.preventDefault());
    btn.addEventListener('click', () => insertToken(entry.token));
    return btn;
  }

  function variablesBar() {
    const entries = placeholderEntries(localCtx, d);
    if (!entries.length) return null;
    const byToken = new Map(entries.map((e) => [e.token, e]));
    const featured = [
      ...entries.filter((e) => e.group === 'feature').slice(0, 8),
      ...FEATURED.map((t) => byToken.get(t)).filter(Boolean),
    ];
    const allId = `${base}-vars`;
    const groups = Object.keys(GROUP_LABELS).map((g) => {
      const list = entries.filter((e) => e.group === g);
      if (!list.length) return null;
      return h('div', { class: 'te-vars-group' }, h('span', { class: 'te-vars-group-title' }, GROUP_LABELS[g]),
        h('div', { class: 'te-chips' }, list.map((e) => chip(e))));
    });
    const all = h('div', { class: 'te-vars-all', id: allId, hidden: true }, groups);
    const more = h('button', { type: 'button', class: 'te-more', 'aria-expanded': 'false', 'aria-controls': allId },
      'All variables', icon('chevron'));
    more.addEventListener('click', () => {
      const open = all.hidden;
      all.hidden = !open;
      more.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
    return h(
      'div',
      { class: 'te-vars', role: 'group', 'aria-label': 'Insert a variable' },
      h('div', { class: 'te-vars-head' }, uiIcon('braces'), h('span', { class: 'te-vars-title' }, 'Variables'),
        h('span', { class: 'te-vars-help' }, 'Click one to insert it at the cursor.')),
      h('div', { class: 'te-chips' }, featured.map((e) => chip(e, true)), more),
      all,
    );
  }

  /* ---- colour ---- */

  function colorRow(e) {
    const id = `${base}-color`;
    const msgId = `${id}-msg`;
    const picker = h('input', { type: 'color', class: 'te-swatch', id: `${id}-picker`, 'aria-label': 'Pick the embed colour' });
    const hex = h('input', {
      type: 'text', id, class: 'te-input te-hex is-mono', maxlength: '7', spellcheck: 'false', autocomplete: 'off',
      placeholder: `${colorHex(env.defaultColor)}`, 'aria-describedby': `${msgId} ${id}-hint`,
    });
    const reset = h('button', { type: 'button', class: 'btn btn-ghost btn-sm te-reset' }, 'Default');
    const msg = h('p', { class: 'te-msg', id: msgId, hidden: true });
    const paint = () => {
      picker.value = colorHex(e.color ?? env.defaultColor).toLowerCase();
      if (document.activeElement !== hex) hex.value = e.color === null ? '' : colorHex(e.color);
      reset.disabled = e.color === null;
      picker.classList.toggle('is-default', e.color === null);
    };
    picker.addEventListener('input', () => {
      e.color = storableColor(parseHex(picker.value) ?? DEFAULT_EMBED_COLOR);
      localErrors.delete('embed.color');
      hex.value = colorHex(e.color);
      paint();
      changed();
    });
    hex.addEventListener('input', () => {
      const v = hex.value.trim();
      if (v === '') {
        e.color = null;
        localErrors.delete('embed.color');
      } else {
        const n = parseHex(v);
        if (n === null) localErrors.set('embed.color', 'Use a hex colour like #5865F2.');
        else {
          e.color = storableColor(n);
          localErrors.delete('embed.color');
        }
      }
      paint();
      changed();
    });
    hex.addEventListener('blur', paint);
    reset.addEventListener('click', () => {
      e.color = null;
      localErrors.delete('embed.color');
      hex.value = '';
      paint();
      changed();
    });
    paint();
    register('embed.color', { input: hex, msg, counter: null, max: 0 });
    return h(
      'div',
      { class: 'te-row' },
      h('div', { class: 'te-label-row' }, h('label', { class: 'te-label', for: id }, 'Colour')),
      h('div', { class: 'te-color' }, picker, hex, reset),
      h('p', { class: 'te-hint', id: `${id}-hint` }, "The bar on the left. Empty uses the server's embed colour."),
      msg,
    );
  }

  /* ---- fields ---- */

  const fieldsHost = h('div', { class: 'te-items' });
  const fieldsCount = h('span', { class: 'te-badge' });
  const addFieldBtn = h('button', { type: 'button', class: 'btn btn-secondary btn-sm te-add' }, icon('plus'), 'Add field');
  addFieldBtn.addEventListener('click', () => {
    if (!state.embed || state.embed.fields.length >= L.fields) return;
    const item = { uid: nextUid(), name: '', value: '', inline: false };
    state.embed.fields.push(item);
    renderFields();
    changed();
    controls.get(`embed.fields.${state.embed.fields.length - 1}.name`)?.input.focus();
  });

  function moveItem(list, idx, delta, rerender, focusSel) {
    const to = idx + delta;
    if (to < 0 || to >= list.length) return;
    const [item] = list.splice(idx, 1);
    list.splice(to, 0, item);
    rerender();
    changed();
    const row = root.querySelector(`[data-uid="${item.uid}"]`);
    const btn = row?.querySelector(focusSel);
    const other = row?.querySelector(focusSel === '.te-up' ? '.te-down' : '.te-up');
    (btn && !btn.disabled ? btn : other && !other.disabled ? other : row?.querySelector('.te-input'))?.focus();
  }

  function removeItem(list, idx, rerender, fallbackFocus) {
    list.splice(idx, 1);
    rerender();
    changed();
    fallbackFocus.focus();
  }

  // Move up / move down / remove for one item of a list (`what` = 'field 2', 'button 1', ...).
  function itemTools(what, list, idx, rerender, addBtn) {
    return h('span', { class: 'te-item-tools' },
      iconButton('up', `Move ${what} up`, () => moveItem(list, idx, -1, rerender, '.te-up'),
        { disabled: idx === 0, cls: 'te-up' }),
      iconButton('down', `Move ${what} down`, () => moveItem(list, idx, 1, rerender, '.te-down'),
        { disabled: idx === list.length - 1, cls: 'te-down' }),
      iconButton('trash', `Remove ${what}`, () => removeItem(list, idx, rerender, addBtn), { cls: 'te-danger' }));
  }

  function renderFields() {
    for (const key of [...controls.keys()]) if (key.startsWith('embed.fields.')) controls.delete(key);
    const e = state.embed;
    if (!e) return;
    fieldsHost.replaceChildren(...e.fields.map((f, idx) => {
      const n = idx + 1;
      return h(
        'div',
        { class: 'te-item', 'data-uid': f.uid, role: 'group', 'aria-label': `Field ${n}` },
        h('div', { class: 'te-item-head' },
          h('span', { class: 'te-item-title' }, `Field ${n}`),
          itemTools(`field ${n}`, e.fields, idx, renderFields, addFieldBtn),
          switchRow(`field-${f.uid}-inline`, 'Inline', f.inline, (v) => { f.inline = v; })),
        textBox({
          path: `embed.fields.${idx}.name`, idKey: `field-${f.uid}-name`, label: 'Name', obj: f, key: 'name', max: L.fieldName,
        }),
        textBox({
          path: `embed.fields.${idx}.value`, idKey: `field-${f.uid}-value`, label: 'Value', obj: f, key: 'value',
          max: L.fieldValue, multiline: true,
        }),
      );
    }));
    fieldsCount.textContent = `${e.fields.length} / ${L.fields}`;
    addFieldBtn.disabled = e.fields.length >= L.fields;
    applyDisabled();
    paintIssues();
  }

  /* ---- buttons ---- */

  const buttonsHost = h('div', { class: 'te-items' });
  const buttonsCount = h('span', { class: 'te-badge' });
  const addButtonBtn = h('button', { type: 'button', class: 'btn btn-secondary btn-sm te-add' }, icon('plus'), 'Add button');
  addButtonBtn.addEventListener('click', () => {
    if (state.buttons.length >= L.buttons) return;
    state.buttons.push({ uid: nextUid(), label: '', url: '', emoji: '' });
    renderButtons();
    changed();
    controls.get(`buttons.${state.buttons.length - 1}.label`)?.input.focus();
  });

  function renderButtons() {
    for (const key of [...controls.keys()]) if (key.startsWith('buttons.')) controls.delete(key);
    buttonsHost.replaceChildren(...state.buttons.map((b, idx) => {
      const n = idx + 1;
      return h(
        'div',
        { class: 'te-item', 'data-uid': b.uid, role: 'group', 'aria-label': `Button ${n}` },
        h('div', { class: 'te-item-head' },
          h('span', { class: 'te-item-title' }, `Button ${n}`),
          itemTools(`button ${n}`, state.buttons, idx, renderButtons, addButtonBtn)),
        h('div', { class: 'te-btn-grid' },
          textBox({
            path: `buttons.${idx}.label`, idKey: `btn-${b.uid}-label`, label: 'Label', obj: b, key: 'label', max: L.buttonLabel,
          }),
          textBox({
            path: `buttons.${idx}.emoji`, idKey: `btn-${b.uid}-emoji`, label: 'Emoji', obj: b, key: 'emoji', placeholder: 'Optional',
          })),
        textBox({
          path: `buttons.${idx}.url`, idKey: `btn-${b.uid}-url`, label: 'Link', obj: b, key: 'url', url: true, placeholder: 'https://',
        }),
      );
    }));
    buttonsCount.textContent = `${state.buttons.length} / ${L.buttons}`;
    addButtonBtn.disabled = state.buttons.length >= L.buttons;
    applyDisabled();
    paintIssues();
  }

  /* ---- embed ---- */

  const totalCount = h('span', { class: 'te-total', id: `${base}-total` });
  const paintTotal = () => {
    if (!state.embed) return;
    const n = embedTextLength(state.embed);
    totalCount.textContent = `${fmt(n)} / ${fmt(L.total)} characters`;
    totalCount.classList.toggle('is-near', n > L.total * 0.9 && n <= L.total);
    totalCount.classList.toggle('is-over', n > L.total);
  };

  function embedBlock() {
    const e = state.embed;
    if (!e) {
      const add = h('button', { type: 'button', class: 'btn btn-secondary te-add-embed' }, icon('plus'), 'Add an embed');
      add.addEventListener('click', () => {
        state.embed = embedState({});
        removedEmbed = null;
        renderEditor();
        changed();
        controls.get('embed.title')?.input.focus();
      });
      const undo = removedEmbed
        ? h('button', { type: 'button', class: 'btn btn-ghost btn-sm' }, uiIcon('undo'), 'Undo remove')
        : null;
      undo?.addEventListener('click', () => {
        state.embed = removedEmbed;
        removedEmbed = null;
        renderEditor();
        changed();
        controls.get('embed.title')?.input.focus();
      });
      const block = h('div', { class: 'te-embed-empty' },
        h('p', { class: 'te-hint' }, 'Embeds add a coloured box with a title, description, fields, images and a footer.'),
        h('div', { class: 'te-embed-empty-actions' }, add, undo));
      return { block, removeBtn: null };
    }
    const removeBtn = embedOnly
      ? null
      : h('button', { type: 'button', class: 'btn btn-ghost btn-sm te-remove-embed' }, uiIcon('trash'), 'Remove embed');
    removeBtn?.addEventListener('click', () => {
      removedEmbed = state.embed;
      state.embed = null;
      for (const key of [...localErrors.keys()]) if (key.startsWith('embed.')) localErrors.delete(key);
      renderEditor();
      changed();
      root.querySelector('.te-add-embed')?.focus();
    });

    const has = (...vals) => vals.some((v) => String(v ?? '').trim() !== '');
    const author = section('Author', { open: has(e.author.name, e.author.iconUrl, e.author.url) });
    author.body.append(
      textBox({ path: 'embed.author.name', idKey: 'author-name', label: 'Name', obj: e.author, key: 'name', max: L.author }),
      h('div', { class: 'te-pair' },
        textBox({
          path: 'embed.author.iconUrl', idKey: 'author-icon', label: 'Icon URL', obj: e.author, key: 'iconUrl',
          url: true, placeholder: '{user.avatar}',
        }),
        textBox({
          path: 'embed.author.url', idKey: 'author-url', label: 'Link', obj: e.author, key: 'url', url: true, placeholder: 'https://',
        })),
    );
    const body = section('Title, description and colour');
    body.body.append(
      colorRow(e),
      textBox({ path: 'embed.title', idKey: 'title', label: 'Title', obj: e, key: 'title', max: L.title }),
      textBox({
        path: 'embed.url', idKey: 'title-url', label: 'Title link', obj: e, key: 'url', url: true, placeholder: 'https://',
        hint: 'Makes the title a link.',
      }),
      textBox({
        path: 'embed.description', idKey: 'description', label: 'Description', obj: e, key: 'description',
        max: L.description, multiline: true,
        hint: 'Markdown works: **bold**, *italic*, __underline__, ~~strike~~, `code`, [links](https://…), '
          + '# headings, - lists, > quotes.',
      }),
    );
    const fields = section('Fields', { open: e.fields.length > 0, badge: fieldsCount });
    const fieldsMsg = h('p', { class: 'te-msg', hidden: true });
    blockMsgs.set('embed.fields', fieldsMsg);
    fields.body.append(fieldsHost, fieldsMsg, h('div', { class: 'te-list-foot' }, addFieldBtn,
      h('span', { class: 'te-hint' }, 'Inline fields sit side by side, up to three per row.')));
    const images = section('Images', { open: has(e.thumbnail, e.image) });
    images.body.append(
      textBox({
        path: 'embed.thumbnail', idKey: 'thumbnail', label: 'Thumbnail URL', obj: e, key: 'thumbnail',
        url: true, placeholder: '{user.avatar}',
        hint: 'Small image at the top right.',
      }),
      textBox({
        path: 'embed.image', idKey: 'image', label: 'Image URL', obj: e, key: 'image', url: true, placeholder: 'https://',
        hint: 'Large image below the text.',
      }),
    );
    const footer = section('Footer', { open: has(e.footer.text, e.footer.iconUrl) || e.timestamp });
    footer.body.append(
      textBox({ path: 'embed.footer.text', idKey: 'footer-text', label: 'Text', obj: e.footer, key: 'text', max: L.footer }),
      textBox({
        path: 'embed.footer.iconUrl', idKey: 'footer-icon', label: 'Icon URL', obj: e.footer, key: 'iconUrl',
        url: true, placeholder: '{server.icon}',
      }),
      switchRow('timestamp', 'Show the time the message was posted', e.timestamp, (v) => { e.timestamp = v; }),
    );
    author.details.dataset.paths = 'embed.author';
    body.details.dataset.paths = 'embed.title embed.url embed.description embed.color';
    fields.details.dataset.paths = 'embed.fields';
    images.details.dataset.paths = 'embed.thumbnail embed.image';
    footer.details.dataset.paths = 'embed.footer';
    const totalMsg = h('p', { class: 'te-msg', hidden: true });
    blockMsgs.set('embed.total', totalMsg);
    paintTotal();
    const block = h('div', { class: 'te-embed' },
      author.details, body.details, fields.details, images.details, footer.details, totalMsg);
    block.style.setProperty('--embed-color', colorHex(e.color ?? env.defaultColor));
    return { block, removeBtn };
  }

  function renderEditor() {
    controls = new Map();
    blockMsgs.clear();
    const parts = [];
    const vars = variablesBar();
    if (vars) parts.push(vars);
    if (!embedOnly) {
      parts.push(h('section', { class: 'te-card' },
        textBox({
          path: 'content', idKey: 'content', label: 'Message text', obj: state, key: 'content', max: L.content, multiline: true,
          placeholder: 'Hey {user}, welcome to {server}!',
          hint: 'Shown above the embed. Mentions here ping (except @everyone and @here).',
        })));
    }
    const eb = embedBlock();
    const embedHead = h('div', { class: 'te-card-head' },
      h('span', { class: 'te-card-title' }, uiIcon('layers'), 'Embed'),
      state.embed ? totalCount : null,
      eb.removeBtn);
    parts.push(h('section', { class: 'te-card te-card-embed' }, embedHead, eb.block));
    if (!embedOnly) {
      const buttonsMsg = h('p', { class: 'te-msg', hidden: true });
      blockMsgs.set('buttons', buttonsMsg);
      parts.push(h('section', { class: 'te-card' },
        h('div', { class: 'te-card-head' },
          h('span', { class: 'te-card-title' }, uiIcon('external'), 'Link buttons'), buttonsCount),
        buttonsHost, buttonsMsg,
        h('div', { class: 'te-list-foot' }, addButtonBtn, h('span', { class: 'te-hint' }, 'Up to 5 buttons per row.'))));
    }
    const emptyMsg = h('p', { class: 'te-msg te-msg-block', hidden: true });
    blockMsgs.set('', emptyMsg);
    parts.push(emptyMsg);
    editCol.replaceChildren(...parts);
    renderFields();
    renderButtons();
    applyDisabled();
    paintIssues();
    schedulePreview();
    if (typeof requestAnimationFrame === 'function') {
      requestAnimationFrame(() => {
        for (const ta of editCol.querySelectorAll('textarea')) autoGrow(ta);
      });
    }
  }

  /* ---- problems ---- */

  function openSectionFor(path) {
    for (const det of editCol.querySelectorAll('details[data-paths]')) {
      if (det.dataset.paths.split(' ').some((p) => path === p || path.startsWith(`${p}.`))) det.open = true;
    }
  }

  function paintIssues() {
    if (destroyed) return;
    const issues = validateState(state, { embedOnly, requireContent, touched: touched || Boolean(serverError) });
    for (const [path, message] of localErrors) issues.unshift({ path, message, level: 'error' });
    const byPath = new Map();
    for (const issue of issues) if (!byPath.has(issue.path)) byPath.set(issue.path, issue);
    if (serverError?.path) byPath.set(serverError.path, { path: serverError.path, message: serverError.message, level: 'error' });

    for (const [path, entry] of controls) {
      const issue = byPath.get(path);
      entry.msg.hidden = !issue;
      entry.msg.textContent = issue ? issue.message : '';
      entry.msg.classList.toggle('is-warn', issue?.level === 'warn');
      if (issue?.level === 'error') entry.input.setAttribute('aria-invalid', 'true');
      else entry.input.removeAttribute('aria-invalid');
      entry.input.classList.toggle('is-warn', issue?.level === 'warn');
    }
    for (const [path, el] of blockMsgs) {
      const issue = byPath.get(path);
      el.hidden = !issue;
      el.textContent = issue ? issue.message : '';
      el.classList.toggle('is-warn', issue?.level === 'warn');
    }
    // A server message that no part shows goes in the box at the top.
    const shown = Boolean(serverError?.path) && (controls.has(serverError.path) || blockMsgs.has(serverError.path));
    serverBox.hidden = !serverError || shown;
    serverBox.replaceChildren(...(serverError ? [icon('alert'), h('span', {}, serverError.message)] : []));
    const errorPaths = [...byPath.values()].filter((i) => i.level === 'error').map((i) => i.path);
    for (const det of editCol.querySelectorAll('details[data-paths]')) {
      const mine = det.dataset.paths.split(' ');
      det.classList.toggle('has-issue', errorPaths.some((p) => mine.some((m) => p === m || p.startsWith(`${m}.`))));
    }
    root.classList.toggle('has-error', errorPaths.length > 0 || Boolean(serverError));
    paintTotal();
    const block = editCol.querySelector('.te-embed');
    if (block && state.embed) block.style.setProperty('--embed-color', colorHex(state.embed.color ?? env.defaultColor));
  }

  renderEditor();

  return {
    el: root,
    getValue,
    setValue(v) {
      state = toState(v, embedOnly);
      touched = false;
      serverError = null;
      localErrors.clear();
      removedEmbed = null;
      renderEditor();
    },
    setError(message, subPath = '') {
      if (!message) {
        serverError = null;
      } else {
        let path = String(subPath ?? '');
        if (embedOnly && path) path = `embed.${path}`;
        if (!path) path = guessPath(message);
        serverError = { message: String(message), path };
        openSectionFor(path);
      }
      paintIssues();
    },
    setResources(resources) {
      localCtx = { ...localCtx, resources };
      env = buildEnv(localCtx, d);
      schedulePreview();
    },
    focus() {
      const target = serverError ? controls.get(serverError.path)?.input : null;
      const first = editCol.querySelector('[aria-invalid="true"]');
      const fallback = controls.get(embedOnly ? 'embed.title' : 'content')?.input
        ?? editCol.querySelector('input, textarea, button');
      (target ?? first ?? fallback)?.focus();
      if (target || first) showPane('edit');
    },
    destroy() {
      destroyed = true;
      if (frame && typeof cancelAnimationFrame === 'function') cancelAnimationFrame(frame);
      frame = 0;
    },
  };
}
