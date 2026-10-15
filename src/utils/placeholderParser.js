// ── Placeholder engine ─────────────────────────────────────────────────────────
// One variable set for every message (welcome, tags, templates, feeds, ...): {user.name},
// {server.member_count}, {channel}, {random:1:100}, argument tokens {1} {1:default} {2...},
// and feature variables passed in `ctx.vars`. Replacement is a single pass with a function
// replacer, so substituted values are never re-scanned and `$&` stays literal.
// None of these functions ever throws.
import { ChannelType } from 'discord.js';
import logger from './logger.js';
import { clampText, ordinal } from './text.js';

/**
 * @typedef {{ member?: import('discord.js').GuildMember|null, user?: import('discord.js').User|null,
 *   guild?: import('discord.js').Guild|null, channel?: import('discord.js').Channel|null,
 *   vars?: Record<string, string|number|boolean|null|(() => string)>, args?: string[] }} RenderContext
 */

const TOKEN_RE = /\{([^{}\n]{1,64})\}/g;
const MAX_UNKNOWN_TOKENS = 50;
const IMAGE_OPTS = Object.freeze({ extension: 'png', size: 256 });

// ── Built-in tokens ────────────────────────────────────────────────────────────

const stamp = (ms, style) => (Number.isFinite(ms) ? `<t:${Math.floor(ms / 1000)}:${style}>` : '');

// A display name or nickname is free text: "<@123>" in one would render (and, with parse: ['users'], ping) as a mention.
// A zero-width space after "<" keeps the text as typed but no longer a mention, role or channel tag.
const unmention = (s) => String(s).replace(/<(?=[@#])/g, '<\u200b');

function channelTypeName(type) {
  switch (type) {
    case ChannelType.GuildText: return 'text';
    case ChannelType.GuildVoice: return 'voice';
    case ChannelType.GuildCategory: return 'category';
    case ChannelType.GuildAnnouncement: return 'announcement';
    case ChannelType.GuildStageVoice: return 'stage';
    case ChannelType.GuildForum: return 'forum';
    case ChannelType.GuildMedia: return 'media';
    case ChannelType.PublicThread:
    case ChannelType.PrivateThread:
    case ChannelType.AnnouncementThread: return 'thread';
    default: return 'other';
  }
}

// [aliases, namespace, description, getter({ u, m, g, c })]
const BUILTINS = [
  [['{user}', '{user.mention}'], 'user', 'Mention of the member', ({ u }) => `<@${u.id}>`],
  [['{user.name}', '{user.username}'], 'user', 'Username', ({ u }) => u.username ?? ''],
  [['{user.idname}', '{user.tag}'], 'user', 'User tag (the username when there is no discriminator)', ({ u }) => (
    typeof u.tag === 'string' ? u.tag
      : (u.discriminator && u.discriminator !== '0' ? `${u.username}#${u.discriminator}` : u.username ?? '')
  )],
  [['{user.id}'], 'user', 'User ID', ({ u }) => u.id],
  [['{user.avatar}', '{user.avatar_url}'], 'user', 'Avatar URL', ({ u, m }) => (m ?? u).displayAvatarURL?.(IMAGE_OPTS) ?? ''],
  [['{user.bot}'], 'user', 'True or False', ({ u }) => (u.bot ? 'True' : 'False')],
  [['{user.display_name}', '{user.nick}', '{user.nickname}'], 'user', 'Server nickname, falling back to the display name',
    ({ u, m }) => unmention(m?.displayName ?? u.globalName ?? u.username ?? '')],
  [['{user.created}'], 'user', 'Account age, relative', ({ u }) => stamp(u.createdTimestamp, 'R')],
  [['{user.created_at}'], 'user', 'Account creation date', ({ u }) => stamp(u.createdTimestamp, 'D')],
  [['{user.joined}'], 'user', 'Time since joining the server, relative', ({ m }) => stamp(m?.joinedTimestamp, 'R')],
  [['{user.joined_at}'], 'user', 'Date the member joined the server', ({ m }) => stamp(m?.joinedTimestamp, 'D')],
  [['{server}', '{server.name}'], 'server', 'Server name', ({ g }) => g.name ?? ''],
  [['{server.id}'], 'server', 'Server ID', ({ g }) => g.id],
  [['{server.icon}', '{server.icon_url}'], 'server', 'Server icon URL', ({ g }) => g.iconURL?.(IMAGE_OPTS) ?? ''],
  [['{server.owner}'], 'server', 'Mention of the server owner', ({ g }) => (g.ownerId ? `<@${g.ownerId}>` : '')],
  [['{server.owner_id}'], 'server', 'ID of the server owner', ({ g }) => g.ownerId ?? ''],
  [['{server.member_count}'], 'server', 'Member count, e.g. 1,234', ({ g }) => Number(g.memberCount ?? 0).toLocaleString('en-US')],
  [['{server.member_count.ordinal}'], 'server', 'Member count as an ordinal, e.g. 1,234th', ({ g }) => ordinal(Number(g.memberCount ?? 0))],
  [['{server.verification_level}'], 'server', 'Verification level (0-4)', ({ g }) => String(g.verificationLevel ?? '')],
  [['{server.joined_at}'], 'server', 'Date the bot joined the server', ({ g }) => stamp(g.joinedTimestamp, 'D')],
  [['{server.created_at}'], 'server', 'Date the server was created', ({ g }) => stamp(g.createdTimestamp, 'D')],
  [['{server.boost_count}'], 'server', 'Number of boosts', ({ g }) => String(g.premiumSubscriptionCount ?? 0)],
  [['{server.boost_level}'], 'server', 'Boost tier', ({ g }) => String(g.premiumTier ?? 0)],
  [['{channel}', '{channel.mention}'], 'channel', 'Mention of the channel', ({ c }) => `<#${c.id}>`],
  [['{channel.name}'], 'channel', 'Channel name', ({ c }) => c.name ?? ''],
  [['{channel.id}'], 'channel', 'Channel ID', ({ c }) => c.id],
  [['{channel.type}'], 'channel', 'Channel type: text, voice, category, announcement, stage, forum, media, thread or other',
    ({ c }) => channelTypeName(c.type)],
];

/** token (lowercase, no braces) → { ns, get } */
const BUILTIN_MAP = new Map();
const descriptions = {};
for (const [aliases, ns, description, get] of BUILTINS) {
  for (const alias of aliases) {
    BUILTIN_MAP.set(alias.slice(1, -1), { ns, get });
    descriptions[alias] = description;
  }
}
descriptions['{random}'] = 'Random number from 0 to 10';
descriptions['{random:x:y}'] = 'Random number from x to y, e.g. {random:1:100}';

/** Every built-in token and alias with a short description. */
export const PLACEHOLDERS = Object.freeze(descriptions);

// ── Rendering ──────────────────────────────────────────────────────────────────

const ARG_PATTERNS = {
  single: /^(\d{1,3})$/,
  withDefault: /^(\d{1,3}):([\s\S]*)$/,
  from: /^(\d{1,3})\.\.\.$/,
  upTo: /^\.\.\.(\d{1,3})$/,
  range: /^(\d{1,3})\.\.\.(\d{1,3})$/,
  all: /^\.\.\.$/,
};
const RANDOM_RANGE_RE = /^random:(-?\d{1,10}):(-?\d{1,10})$/;

const clampInt = (n) => Math.max(-1e9, Math.min(1e9, n));

function normaliseVarKey(key) {
  let k = String(key).trim();
  if (k.startsWith('{') && k.endsWith('}')) k = k.slice(1, -1);
  return k.toLowerCase();
}

/**
 * Renders placeholders and reports what could not be resolved.
 * @param {unknown} template
 * @param {RenderContext} [ctx]
 * @param {{ maxLength?: number }} [opts]
 * @returns {{ text: string, missingArgs: number[], unknownTokens: string[] }}
 *   `missingArgs`: argument numbers that were asked for but not given (sorted).
 *   `unknownTokens`: tokens left untouched because nothing knows them (with braces, original case).
 */
export function renderPlaceholdersDetailed(template, ctx = {}, { maxLength } = {}) {
  if (template === null || template === undefined || template === '') {
    return { text: '', missingArgs: [], unknownTokens: [] };
  }
  const source = typeof template === 'string' ? template : String(template);
  try {
    const c = ctx && typeof ctx === 'object' ? ctx : {};
    const varMap = new Map();
    for (const [key, value] of Object.entries(c.vars && typeof c.vars === 'object' ? c.vars : {})) {
      varMap.set(normaliseVarKey(key), value);
    }
    const args = Array.isArray(c.args) ? c.args.map((a) => String(a ?? '')) : null;
    const objects = {
      u: c.member?.user ?? c.user ?? null,
      m: c.member ?? null,
      g: c.guild ?? c.member?.guild ?? null,
      c: c.channel ?? null,
    };
    const nsObject = { user: objects.u, server: objects.g, channel: objects.c };
    const missing = new Set();
    const unknown = [];

    const resolveArg = (token) => {
      let m;
      if ((m = ARG_PATTERNS.single.exec(token))) {
        const n = Number(m[1]);
        if (n < 1) return undefined;
        if (n > args.length) { missing.add(n); return ''; }
        return args[n - 1];
      }
      if ((m = ARG_PATTERNS.withDefault.exec(token))) {
        const n = Number(m[1]);
        if (n < 1) return undefined;
        const given = args[n - 1];
        return given !== undefined && given !== '' ? given : m[2];
      }
      if ((m = ARG_PATTERNS.from.exec(token))) {
        const n = Number(m[1]);
        return n < 1 ? undefined : args.slice(n - 1).join(' ');
      }
      if ((m = ARG_PATTERNS.upTo.exec(token))) {
        const n = Number(m[1]);
        return n < 1 ? undefined : args.slice(0, n).join(' ');
      }
      if ((m = ARG_PATTERNS.range.exec(token))) {
        const a = Number(m[1]);
        const b = Number(m[2]);
        if (a < 1 || b < 1) return undefined;
        return a <= b ? args.slice(a - 1, b).join(' ') : '';
      }
      if (ARG_PATTERNS.all.test(token)) return args.join(' ');
      return undefined;
    };

    const resolve = (inner) => {
      const name = inner.trim().toLowerCase();

      if (varMap.has(name)) {
        const value = varMap.get(name);
        if (typeof value === 'function') {
          try {
            const out = value();
            return out === null || out === undefined ? '' : String(out);
          } catch (err) {
            logger.warn(`Placeholder var {${name}} threw: ${err?.message}`);
            return '';
          }
        }
        return value === null || value === undefined ? '' : String(value);
      }

      if (args) {
        const out = resolveArg(inner.trim());
        if (out !== undefined) return out;
      }

      if (name === 'random') return String(Math.floor(Math.random() * 11));
      const rnd = RANDOM_RANGE_RE.exec(name);
      if (rnd) {
        let x = clampInt(Number(rnd[1]));
        let y = clampInt(Number(rnd[2]));
        if (x > y) [x, y] = [y, x];
        return String(x + Math.floor(Math.random() * (y - x + 1)));
      }

      const builtin = BUILTIN_MAP.get(name);
      if (builtin) {
        const owner = nsObject[builtin.ns];
        if (!owner) return undefined; // a known token without its context stays untouched
        try {
          return String(builtin.get(objects) ?? '');
        } catch {
          return '';
        }
      }
      return undefined;
    };

    let text = source.replace(TOKEN_RE, (whole, inner) => {
      const out = resolve(inner);
      if (out !== undefined) return out;
      const isBuiltin = BUILTIN_MAP.has(inner.trim().toLowerCase());
      if (!isBuiltin && unknown.length < MAX_UNKNOWN_TOKENS && !unknown.includes(whole)) unknown.push(whole);
      return whole;
    });

    if (Number.isInteger(maxLength) && maxLength > 0) text = clampText(text, maxLength);
    return { text, missingArgs: [...missing].sort((a, b) => a - b), unknownTokens: unknown };
  } catch (err) {
    logger.warn(`renderPlaceholders failed: ${err?.message}`);
    return { text: source, missingArgs: [], unknownTokens: [] };
  }
}

/**
 * Renders placeholders in a template.
 * @param {unknown} template
 * @param {RenderContext} [ctx]
 * @param {{ maxLength?: number }} [opts] - clamp the result with clampText
 * @returns {string}
 */
export function renderPlaceholders(template, ctx = {}, opts = {}) {
  return renderPlaceholdersDetailed(template, ctx, opts).text;
}

/**
 * Legacy wrapper: renders with a member and a guild. A null member leaves the user tokens untouched.
 * @param {string} template
 * @param {import('discord.js').GuildMember|null} member
 * @param {import('discord.js').Guild|null} guild
 * @returns {string}
 */
export function parsePlaceholders(template, member, guild) {
  return renderPlaceholders(template, { member, guild });
}

// ── Help text ──────────────────────────────────────────────────────────────────

const HELP = Object.freeze({
  user: ['**User**',
    '`{user}` `{user.mention}` — mention',
    '`{user.name}` `{user.username}` — username',
    '`{user.display_name}` `{user.nick}` — display name',
    '`{user.idname}` `{user.tag}` — user tag',
    '`{user.id}` — user ID',
    '`{user.avatar_url}` `{user.avatar}` — avatar URL',
    '`{user.bot}` — True / False',
    '`{user.created}` `{user.created_at}` — account age / date',
    '`{user.joined}` `{user.joined_at}` — joined server (relative / date)'],
  server: ['**Server**',
    '`{server}` `{server.name}` — server name',
    '`{server.id}` — server ID',
    '`{server.icon_url}` `{server.icon}` — icon URL',
    '`{server.owner}` `{server.owner_id}` — owner mention / ID',
    '`{server.member_count}` — member count',
    '`{server.member_count.ordinal}` — e.g. 1,234th',
    '`{server.boost_count}` `{server.boost_level}` — boosts / tier',
    '`{server.verification_level}` — verification level (0–4)',
    '`{server.created_at}` `{server.joined_at}` — created / bot joined'],
  channel: ['**Channel**',
    '`{channel}` — mention · `{channel.name}` · `{channel.id}` · `{channel.type}`'],
  random: ['**Random**',
    '`{random}` — number 0–10 · `{random:1:100}` — number from 1 to 100'],
  args: ['**Arguments**',
    '`{1}` first word · `{1:default}` with a fallback · `{2...}` 2nd onwards',
    '`{...3}` words 1–3 · `{2...4}` words 2–4 · `{...}` everything'],
});

/**
 * Markdown help text for the placeholder groups (each block is at most 1024 characters, so it
 * fits one embed field).
 * @param {string[]} [groups] - any of 'user', 'server', 'channel', 'random', 'args'
 * @returns {string}
 */
export function placeholderHelp(groups = ['user', 'server', 'channel']) {
  const chosen = Array.isArray(groups) ? groups : ['user', 'server', 'channel'];
  return chosen
    .filter((g) => typeof g === 'string' && Object.hasOwn(HELP, g))
    .map((g) => HELP[g].join('\n'))
    .join('\n\n');
}

/** Legacy help text for /welcome and /tag. */
export const PLACEHOLDER_LIST = placeholderHelp(['user', 'server']);
