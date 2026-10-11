// The dashboard's HTTP server: JSON API (settings, server resources, per-server records), OAuth2 login, static UI
// files and security headers.
// createDashboardServer() returns a node:http Server that is not listening yet; dashboard.js (repo root)
// starts it. Settings, records and Discord access are injected through `deps`, so this file never opens Redis
// or Postgres connections itself.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import defaultLogger from '../utils/logger.js';
import {
  SESSION_COOKIE, STATE_COOKIE, SESSION_MAX_AGE_SECONDS, STATE_MAX_AGE_SECONDS,
  buildAuthorizeUrl, canManageGuild, checkWriteRequest, clearCookie, createOAuthState, deriveSessionKey,
  parseCookies, sealSession, serializeCookie, statesMatch, unsealSession,
} from './auth.js';
import { buildCatalog } from './catalog.js';
import { DiscordApiError, guildIconUrl, userAvatarUrl } from './discordApi.js';
import { FieldError, checkRefs, collectStoredRefs } from './schema.js';
import { collectSettingsRefs, readSettingsValues, validateSettingsChanges } from './fields.js';
import { COLLECTIONS, RECORD_ID_RE, validateRecordInput } from './collections/index.js';
import { RecordError } from './collections/errors.js';

export const PUBLIC_DIR = fileURLToPath(new URL('./public/', import.meta.url));
/** Body limit of a settings PATCH (a full page of messages and embeds fits). */
export const BODY_LIMIT_BYTES = 256 * 1024;
/** Body limit of a record write. */
export const RECORD_BODY_LIMIT_BYTES = 128 * 1024;
export const PROPAGATION_SECONDS = 30;
export const WRITE_RATE_LIMIT = Object.freeze({ max: 20, windowMs: 10_000 });
const RATE_LIMIT_KEYS_MAX = 10_000;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const ACTION_ID_RE = /^[A-Za-z][A-Za-z0-9_-]{0,39}$/;
const LOGIN_FAILED = '/?error=login_failed';
const STATE_PATH = '/auth';

/** The only files served from PUBLIC_DIR: request path -> [file name, content type]. */
const STATIC_FILES = new Map([
  ['/', ['index.html', 'text/html; charset=utf-8']],
  ['/index.html', ['index.html', 'text/html; charset=utf-8']],
  ['/app.js', ['app.js', 'text/javascript; charset=utf-8']],
  ['/lib.js', ['lib.js', 'text/javascript; charset=utf-8']],
  ['/forms.js', ['forms.js', 'text/javascript; charset=utf-8']],
  ['/collections.js', ['collections.js', 'text/javascript; charset=utf-8']],
  ['/embed-editor.js', ['embed-editor.js', 'text/javascript; charset=utf-8']],
  ['/styles.css', ['styles.css', 'text/css; charset=utf-8']],
  ['/forms.css', ['forms.css', 'text/css; charset=utf-8']],
  ['/collections.css', ['collections.css', 'text/css; charset=utf-8']],
  ['/favicon.svg', ['favicon.svg', 'image/svg+xml']],
]);

export const SECURITY_HEADERS = Object.freeze({
  'Content-Security-Policy': "default-src 'self'; img-src 'self' https://cdn.discordapp.com data:; style-src 'self'; "
    + "script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
  'X-Content-Type-Options': 'nosniff',
  'Referrer-Policy': 'same-origin',
  'X-Frame-Options': 'DENY',
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Permissions-Policy': 'camera=(), microphone=(), geolocation=()',
});

const nameCollator = new Intl.Collator('en', { sensitivity: 'base', numeric: true });
// Strips control characters from user-controlled text before it reaches a log line.
const forLog = (s) => String(s ?? '').replace(/[\u0000-\u001f\u007f]/g, '').slice(0, 100);

/**
 * A sliding-window rate limiter kept in memory (bounded number of keys; the oldest key is dropped first).
 * @param {{ max: number, windowMs: number, now: () => number }} opts
 * @returns {(key: string) => { ok: true } | { ok: false, retryAfter: number }} retryAfter in seconds
 */
export function createRateLimiter({ max, windowMs, now }) {
  const hits = new Map();
  return (key) => {
    const t = now();
    const recent = (hits.get(key) ?? []).filter((at) => at > t - windowMs);
    hits.delete(key);
    if (recent.length >= max) {
      hits.set(key, recent);
      return { ok: false, retryAfter: Math.max(1, Math.ceil((recent[0] + windowMs - t) / 1000)) };
    }
    recent.push(t);
    hits.set(key, recent);
    while (hits.size > RATE_LIMIT_KEYS_MAX) hits.delete(hits.keys().next().value);
    return { ok: true };
  };
}

const formatColor = (n) => (Number.isInteger(n) && n >= 0 && n <= 0xffffff
  ? `#${n.toString(16).padStart(6, '0').toUpperCase()}`
  : null);

/** RecordError code -> the `error` of the answer. */
const RECORD_ERROR_CODES = Object.freeze({
  invalid: 'invalid_request', not_found: 'not_found', conflict: 'conflict', limit: 'limit', discord: 'discord_failed',
  unavailable: 'unavailable',
});

/** The GET /api/guilds/:id/resources payload: getGuildResources without the server-only parts. */
function publicResources(res) {
  return {
    channels: res.channels.map((c) => ({
      id: c.id, name: c.name, type: c.type, kind: c.kind, parentId: c.parentId, position: c.position, nsfw: c.nsfw,
    })),
    roles: res.roles.map((r) => ({
      id: r.id, name: r.name, color: r.color, position: r.position, managed: r.managed, everyone: r.everyone,
      mentionable: r.mentionable, assignable: r.assignable,
    })),
    emojis: res.emojis.map((e) => ({ id: e.id, name: e.name, animated: e.animated })),
    bot: { userId: res.bot.userId, topRolePosition: res.bot.topRolePosition },
  };
}

/** getGuildResources in the shape schema.checkRefs takes. */
function refResources(res) {
  return {
    channels: new Map(res.channels.map((c) => [c.id, { kind: c.kind }])),
    roles: new Map(res.roles.map((r) => [r.id, r])),
  };
}

const knowsAll = (refs, map, keep) => refs.channels.every((r) => map.channels.has(r.id) || keep.channels.has(r.id))
  && refs.roles.every((r) => map.roles.has(r.id) || keep.roles.has(r.id));

function isValidSession(s) {
  return s !== null && typeof s === 'object'
    && typeof s.uid === 'string' && SNOWFLAKE_RE.test(s.uid)
    && typeof s.accessToken === 'string' && s.accessToken.length > 0 && s.accessToken.length <= 512
    && typeof s.username === 'string'
    && (s.globalName === null || typeof s.globalName === 'string')
    && (s.avatar === null || typeof s.avatar === 'string');
}

/**
 * Reads a request body up to `limit` bytes.
 * @returns {Promise<{ ok: true, text: string } | { ok: false, status: number }>}
 */
function readBody(req, limit) {
  return new Promise((resolve) => {
    const declared = Number(req.headers['content-length']);
    if (Number.isFinite(declared) && declared > limit) {
      resolve({ ok: false, status: 413 });
      return;
    }
    const chunks = [];
    let size = 0;
    let done = false;
    const finish = (result) => {
      if (done) return;
      done = true;
      resolve(result);
    };
    req.on('data', (chunk) => {
      if (done) return;
      size += chunk.length;
      if (size > limit) finish({ ok: false, status: 413 });
      else chunks.push(chunk);
    });
    req.on('end', () => finish({ ok: true, text: Buffer.concat(chunks).toString('utf8') }));
    req.on('error', () => finish({ ok: false, status: 400 }));
    req.on('close', () => finish({ ok: false, status: 400 }));
  });
}

/**
 * Builds the dashboard HTTP server (not listening).
 * @param {object} opts
 * @param {ReturnType<import('./config.js').loadDashboardConfig>['config']} opts.config
 * @param {{
 *   discord: ReturnType<import('./discordApi.js').createDiscordApi>,
 *   settings: { readGuildSettings: (guildId: string) => Promise<object>,
 *               writeSettings: (guildId: string, changes: object, ctx: object) => Promise<{ row: object, notices?: string[] }> },
 *   records?: ReturnType<import('./recordsBridge.js').createRecordsBridge>,
 *   collections?: { COLLECTIONS: Record<string, object>, validateRecordInput: Function },
 *   now?: () => number,
 * }} opts.deps  collections: the record registry (tests inject one; default src/dashboard/collections/index.js)
 * @param {object} [opts.logger] info/warn/error, defaults to the bot's logger
 * @param {string} [opts.publicDir] directory of the static UI files
 * @returns {import('node:http').Server}
 */
export function createDashboardServer({ config, deps, logger = defaultLogger, publicDir = PUBLIC_DIR }) {
  const { discord, settings, records = null } = deps;
  const registry = deps.collections ?? { COLLECTIONS, validateRecordInput };
  const now = deps.now ?? Date.now;
  const sessionKey = deriveSessionKey(config.sessionSecret);
  const secure = config.secureCookies === true;
  const baseHeaders = secure
    ? { ...SECURITY_HEADERS, 'Strict-Transport-Security': 'max-age=31536000' }
    : { ...SECURITY_HEADERS };
  const catalogJson = JSON.stringify(buildCatalog());
  const limitWrites = createRateLimiter({ ...WRITE_RATE_LIMIT, now });

  // ── Response helpers ────────────────────────────────────────────────────────

  function send(ctx, status, body, headers = {}) {
    const out = { ...baseHeaders, ...headers };
    if (ctx.cookies.length > 0) out['Set-Cookie'] = ctx.cookies;
    const bodiless = status === 204 || status === 304; // no body, so no Content-Length either
    if (!bodiless) out['Content-Length'] = body === undefined ? 0 : Buffer.byteLength(body);
    ctx.res.writeHead(status, out);
    ctx.res.end(bodiless || ctx.req.method === 'HEAD' ? undefined : body);
  }

  const json = (ctx, status, payload, headers = {}) => send(ctx, status, JSON.stringify(payload), {
    'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store', ...headers,
  });

  const fail = (ctx, status, error, message, extra = {}, headers = {}) => json(ctx, status, { error, message, ...extra }, headers);

  const redirect = (ctx, location) => send(ctx, 302, undefined, { Location: location, 'Cache-Control': 'no-store' });

  function allow(ctx, methods) {
    if (methods.includes(ctx.req.method)) return true;
    fail(ctx, 405, 'method_not_allowed', 'Method not allowed.', {}, { Allow: methods.join(', ') });
    return false;
  }

  // ── Session ─────────────────────────────────────────────────────────────────

  function readSession(ctx) {
    const raw = ctx.cookieIn.get(SESSION_COOKIE);
    if (raw === undefined) return { session: null, stale: false };
    const payload = unsealSession(raw, sessionKey, now());
    return isValidSession(payload) ? { session: payload, stale: false } : { session: null, stale: true };
  }

  const clearSession = (ctx) => ctx.cookies.push(clearCookie(SESSION_COOKIE, { secure }));

  function requireSession(ctx) {
    const { session, stale } = readSession(ctx);
    if (session) return session;
    if (stale) clearSession(ctx);
    fail(ctx, 401, 'unauthenticated', 'Please log in with Discord.');
    return null;
  }

  /** Answers a failed Discord call. `userToken`: the call used the session's token (a 401 ends the session). */
  function discordFailure(ctx, err, userToken) {
    if (err instanceof DiscordApiError && err.code === 'unauthenticated' && userToken) {
      clearSession(ctx);
      return fail(ctx, 401, 'unauthenticated', 'Your Discord login has expired. Please log in again.');
    }
    if (err instanceof DiscordApiError && err.code === 'rate_limited') {
      const retryAfter = err.retryAfter ?? 1;
      return fail(ctx, 429, 'rate_limited', `Discord is rate limiting requests. Try again in ${retryAfter} s.`,
        { retryAfter }, { 'Retry-After': String(retryAfter) });
    }
    if (err instanceof DiscordApiError) logger.warn(`Dashboard: Discord request failed: ${err.message}`);
    else logger.error('Dashboard: Discord request failed', err);
    return fail(ctx, 502, 'discord_unavailable', 'Discord could not be reached. Try again in a moment.');
  }

  /**
   * The user's OAuth guild list; on a Discord failure answers and returns null. `allowStale` lets a list up to
   * 5 minutes old stand in during a Discord 429 or outage: for display only, never for an access decision.
   */
  async function userGuilds(ctx, session, { allowStale = false } = {}) {
    let guilds;
    try {
      guilds = await discord.getUserGuilds(session.uid, session.accessToken, { allowStale });
    } catch (err) {
      discordFailure(ctx, err, true);
      return null;
    }
    if (!Array.isArray(guilds)) throw new TypeError('getUserGuilds returned no list');
    return guilds;
  }

  /** The OAuth guild entry when the user may manage the guild and the bot is in it; otherwise answers and returns null. */
  async function managedGuild(ctx, session, guildId) {
    const guilds = await userGuilds(ctx, session);
    if (!guilds) return null;
    const guild = guilds.find((g) => g.id === guildId);
    if (!canManageGuild(guild, session.uid, config.botOwnerId)) {
      fail(ctx, 403, 'forbidden', 'You need the Manage Server or Administrator permission in this server.');
      return null;
    }
    let present;
    try {
      present = await discord.isBotInGuild(guildId);
    } catch (err) {
      discordFailure(ctx, err, false);
      return null;
    }
    if (!present) {
      fail(ctx, 404, 'bot_not_in_guild', 'The bot is not in this server yet.', { inviteUrl: discord.inviteUrl(guildId) });
      return null;
    }
    return guild;
  }

  // ── Routes ──────────────────────────────────────────────────────────────────

  function login(ctx) {
    if (!allow(ctx, ['GET'])) return;
    const state = createOAuthState();
    ctx.cookies.push(serializeCookie(STATE_COOKIE, state, { maxAge: STATE_MAX_AGE_SECONDS, path: STATE_PATH, secure }));
    redirect(ctx, buildAuthorizeUrl(config, state));
  }

  async function callback(ctx) {
    if (!allow(ctx, ['GET'])) return;
    const q = ctx.query;
    const expected = ctx.cookieIn.get(STATE_COOKIE);
    ctx.cookies.push(clearCookie(STATE_COOKIE, { path: STATE_PATH, secure }));
    const failed = (reason) => {
      logger.warn(`Dashboard login failed: ${reason}`);
      redirect(ctx, LOGIN_FAILED);
    };
    if (q.has('error')) return failed(`Discord answered "${forLog(q.get('error'))}"`);
    if (!statesMatch(expected, q.get('state'))) return failed('the state did not match (expired or forged login)');
    const code = q.get('code');
    if (!code || code.length > 512 || !/^[\w.~-]+$/.test(code)) return failed('missing or malformed code');

    let token;
    let user;
    try {
      token = await discord.exchangeCode(code);
      user = await discord.getCurrentUser(token.accessToken);
    } catch (err) {
      return failed(forLog(err?.message ?? err));
    }
    const issued = now();
    const exp = issued + Math.min(token.expiresIn, SESSION_MAX_AGE_SECONDS) * 1000;
    const sealed = sealSession({
      uid: user.id, username: user.username, globalName: user.globalName, avatar: user.avatar,
      accessToken: token.accessToken, exp,
    }, sessionKey);
    ctx.cookies.push(serializeCookie(SESSION_COOKIE, sealed, { maxAge: (exp - issued) / 1000, secure }));
    logger.info(`Dashboard login: ${forLog(user.username)} (${user.id})`);
    return redirect(ctx, '/');
  }

  function logout(ctx) {
    if (!allow(ctx, ['POST'])) return;
    const check = checkWriteRequest(ctx.req, config);
    if (!check.ok) return fail(ctx, check.status, check.error, check.message);
    const { session } = readSession(ctx);
    clearSession(ctx);
    if (session) {
      discord.forgetUser?.(session.uid);
      Promise.resolve()
        .then(() => discord.revokeToken?.(session.accessToken))
        .catch(() => {});
      logger.info(`Dashboard logout: ${session.uid}`);
    }
    return send(ctx, 204, undefined, { 'Cache-Control': 'no-store' });
  }

  function me(ctx) {
    if (!allow(ctx, ['GET'])) return;
    const session = requireSession(ctx);
    if (!session) return;
    json(ctx, 200, {
      user: {
        id: session.uid,
        username: session.username,
        globalName: session.globalName,
        avatarUrl: userAvatarUrl({ id: session.uid, avatar: session.avatar }),
      },
    });
  }

  function catalog(ctx) {
    if (!allow(ctx, ['GET'])) return;
    if (!requireSession(ctx)) return;
    send(ctx, 200, catalogJson, { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' });
  }

  async function guildList(ctx) {
    if (!allow(ctx, ['GET'])) return;
    const session = requireSession(ctx);
    if (!session) return;
    const guilds = await userGuilds(ctx, session, { allowStale: true });
    if (!guilds) return;
    const manageable = guilds.filter((g) => canManageGuild(g, session.uid, config.botOwnerId));
    let present;
    try {
      present = await Promise.all(manageable.map((g) => discord.isBotInGuild(g.id, { verify: false })));
    } catch (err) {
      discordFailure(ctx, err, false);
      return;
    }
    const list = manageable.map((g, i) => ({
      id: g.id,
      name: g.name,
      iconUrl: guildIconUrl(g),
      botPresent: present[i] === true,
      inviteUrl: present[i] === true ? null : discord.inviteUrl(g.id),
    }));
    list.sort((a, b) => (Number(b.botPresent) - Number(a.botPresent))
      || nameCollator.compare(a.name, b.name)
      || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    json(ctx, 200, { guilds: list });
  }

  /** Answers 429 and returns false when the user is over the write rate limit. */
  function withinWriteLimit(ctx, session) {
    const limited = limitWrites(session.uid);
    if (limited.ok) return true;
    fail(ctx, 429, 'rate_limited', `Too many changes at once. Try again in ${limited.retryAfter} s.`,
      { retryAfter: limited.retryAfter }, { 'Retry-After': String(limited.retryAfter) });
    return false;
  }

  /** Reads and parses a JSON body; on failure answers and returns undefined. */
  async function readJson(ctx, limit, { optional = false } = {}) {
    const body = await readBody(ctx.req, limit);
    if (!body.ok) {
      if (body.status === 413) fail(ctx, 413, 'invalid_request', 'The request body is too large.', {}, { Connection: 'close' });
      else fail(ctx, 400, 'invalid_request', 'The request body could not be read.');
      return undefined;
    }
    if (optional && body.text.trim() === '') return {};
    try {
      return JSON.parse(body.text);
    } catch {
      fail(ctx, 400, 'invalid_request', 'The request body is not valid JSON.');
      return undefined;
    }
  }

  /**
   * Checks the channel and role ids of a write against the server: they must exist with the right kind (ids the stored
   * value already held may stay), and a role the bot will give must be one it can give and one the user could give
   * (checkAssignableRole). Answers and returns false on a problem.
   * @param {import('./schema.js').Refs} refs
   * @param {{ channels: Set<string>, roles: Set<string> }} keep
   */
  async function verifyRefs(ctx, session, guildId, refs, keep) {
    if (refs.channels.length === 0 && refs.roles.length === 0) return true;
    let res;
    try {
      res = await discord.getGuildResources(guildId);
      // An id the cached lists do not know may be a channel or role created in the last few seconds.
      if (!knowsAll(refs, refResources(res), keep)) res = await discord.getGuildResources(guildId, { fresh: true });
    } catch (err) {
      discordFailure(ctx, err, false);
      return false;
    }
    const map = refResources(res);
    try {
      checkRefs(refs, map, { keepChannels: keep.channels, keepRoles: keep.roles });
    } catch (err) {
      if (!(err instanceof FieldError)) throw err;
      fail(ctx, 400, 'invalid_request', err.message, { path: err.path });
      return false;
    }

    const given = refs.roles.filter((r) => r.assignable && !keep.roles.has(r.id) && map.roles.has(r.id));
    if (given.length === 0) return true;
    const problem = (ref, message) => {
      fail(ctx, 400, 'invalid_request', message, { path: ref.path });
      return false;
    };
    if (!res.bot.manageRoles) return problem(given[0], 'The bot needs the Manage Roles permission to give roles.');
    for (const ref of given) {
      if (map.roles.get(ref.id).dangerous) {
        return problem(ref, 'That role has moderator or admin permissions (such as Administrator, Manage Server, Ban or Kick Members). '
          + 'The bot does not hand those out; pick a role without them.');
      }
    }
    if (res.guild.ownerId === session.uid || (config.botOwnerId && config.botOwnerId === session.uid)) return true;
    let member;
    try {
      member = await discord.getMember(guildId, session.uid);
    } catch (err) {
      discordFailure(ctx, err, false);
      return false;
    }
    const top = Math.max(0, ...(member?.roles ?? []).map((id) => map.roles.get(id)?.position ?? 0));
    const above = given.find((ref) => map.roles.get(ref.id).position >= top);
    return above ? problem(above, 'That role is not below your highest role, so you cannot have the bot give it.') : true;
  }

  async function guildSettings(ctx, guildId) {
    if (!allow(ctx, ['GET', 'PATCH'])) return;
    if (ctx.req.method === 'PATCH') {
      const check = checkWriteRequest(ctx.req, config);
      if (!check.ok) {
        fail(ctx, check.status, check.error, check.message);
        return;
      }
    }
    const session = requireSession(ctx);
    if (!session) return;
    if (!SNOWFLAKE_RE.test(guildId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid server id.');
      return;
    }
    const loadRow = async () => {
      try {
        return { row: await settings.readGuildSettings(guildId) };
      } catch (err) {
        logger.warn(`Dashboard: could not load settings for guild ${guildId}: ${err?.message ?? err}`);
        fail(ctx, 503, 'settings_unavailable', 'The settings could not be loaded. Try again in a moment.');
        return null;
      }
    };

    if (ctx.req.method === 'GET') {
      const guild = await managedGuild(ctx, session, guildId);
      if (!guild) return;
      const loaded = await loadRow();
      if (!loaded) return;
      const { row } = loaded;
      json(ctx, 200, {
        guild: { id: guild.id, name: guild.name, iconUrl: guildIconUrl(guild) },
        values: readSettingsValues(row),
        general: {
          timezone: typeof row?.timezone === 'string' ? row.timezone : 'UTC',
          customPrefix: typeof row?.customPrefix === 'string' ? row.customPrefix : '!',
          embedColor: formatColor(row?.embedColor),
        },
        propagationSeconds: PROPAGATION_SECONDS,
      });
      return;
    }

    // PATCH
    if (!withinWriteLimit(ctx, session)) return;
    const parsed = await readJson(ctx, BODY_LIMIT_BYTES);
    if (parsed === undefined) return;
    const valid = validateSettingsChanges(parsed);
    if (!valid.ok) {
      fail(ctx, 400, 'invalid_request', valid.message, valid.path ? { path: valid.path } : {});
      return;
    }
    const guild = await managedGuild(ctx, session, guildId);
    if (!guild) return;
    if (valid.refs.channels.length > 0 || valid.refs.roles.length > 0) {
      const loaded = await loadRow();
      if (!loaded) return;
      if (!await verifyRefs(ctx, session, guildId, valid.refs, collectSettingsRefs(loaded.row))) return;
    }
    let result;
    try {
      result = await settings.writeSettings(guildId, valid.changes, { discord, logger, userId: session.uid });
    } catch (err) {
      if (err instanceof FieldError) {
        fail(ctx, 400, 'invalid_request', err.message, { path: err.path });
        return;
      }
      logger.error(`Dashboard: saving settings for guild ${guildId} failed`, err);
      fail(ctx, 500, 'save_failed', 'The change could not be saved. Try again in a moment.');
      return;
    }
    // Field names only: values can be long or private.
    logger.info(`Dashboard: user ${session.uid} changed guild ${guildId}: ${Object.keys(valid.changes).join(', ')}`);
    const notices = Array.isArray(result?.notices) ? result.notices.filter((n) => typeof n === 'string') : [];
    json(ctx, 200, { values: readSettingsValues(result?.row), ...(notices.length ? { notices } : {}) });
  }

  async function guildResources(ctx, guildId) {
    if (!allow(ctx, ['GET'])) return;
    const session = requireSession(ctx);
    if (!session) return;
    if (!SNOWFLAKE_RE.test(guildId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid server id.');
      return;
    }
    const fresh = ctx.query.get('fresh') === '1';
    // A forced reload costs Discord requests, so it counts towards the per-user limit.
    if (fresh && !withinWriteLimit(ctx, session)) return;
    const guild = await managedGuild(ctx, session, guildId);
    if (!guild) return;
    let res;
    try {
      res = await discord.getGuildResources(guildId, { fresh });
    } catch (err) {
      discordFailure(ctx, err, false);
      return;
    }
    json(ctx, 200, publicResources(res));
  }

  /** Answers a failed record operation. */
  function recordFailure(ctx, err, { write, what }) {
    if (err instanceof FieldError) return fail(ctx, 400, 'invalid_request', err.message, err.path ? { path: err.path } : {});
    if (err instanceof RecordError) {
      if (err.code === 'discord') logger.warn(`Dashboard: ${what}: ${err.message}`);
      return fail(ctx, err.status, RECORD_ERROR_CODES[err.code], err.message, err.path ? { path: err.path } : {});
    }
    if (err instanceof DiscordApiError) {
      logger.warn(`Dashboard: ${what}: ${err.message}`);
      return fail(ctx, 502, 'discord_failed', 'Discord refused the change. Check the bot\'s permissions and try again.');
    }
    logger.error(`Dashboard: ${what} failed`, err);
    return write
      ? fail(ctx, 500, 'save_failed', 'The change could not be saved. Try again in a moment.')
      : fail(ctx, 503, 'unavailable', 'The list could not be loaded. Try again in a moment.');
  }

  /** Every channel and role id a stored record holds (checkRefs' keep sets). */
  function recordRefs(collection, item) {
    const out = { channels: new Set(), roles: new Set() };
    for (const desc of [...collection.fields, ...(collection.readOnly ?? [])]) collectStoredRefs(desc, item?.[desc.field], out);
    return out;
  }

  async function guildRecords(ctx, guildId, key, itemId, actionId) {
    const method = ctx.req.method;
    const methods = actionId !== undefined ? ['POST'] : itemId !== undefined ? ['GET', 'PATCH', 'DELETE'] : ['GET', 'POST'];
    if (!allow(ctx, methods)) return;
    const write = method !== 'GET';
    if (write) {
      const check = checkWriteRequest(ctx.req, config);
      if (!check.ok) {
        fail(ctx, check.status, check.error, check.message);
        return;
      }
    }
    const session = requireSession(ctx);
    if (!session) return;
    if (!SNOWFLAKE_RE.test(guildId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid server id.');
      return;
    }
    const collection = Object.hasOwn(registry.COLLECTIONS, key) ? registry.COLLECTIONS[key] : null;
    if (!collection) {
      fail(ctx, 404, 'not_found', 'Unknown list.');
      return;
    }
    if (itemId !== undefined && !RECORD_ID_RE.test(itemId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid item id.');
      return;
    }
    if (actionId !== undefined && (!ACTION_ID_RE.test(actionId) || !(collection.actions ?? []).some((a) => a.id === actionId))) {
      fail(ctx, 404, 'not_found', 'Unknown action.');
      return;
    }
    if (!records) {
      fail(ctx, 503, 'unavailable', 'Lists cannot be managed right now.');
      return;
    }
    let data;
    if (write) {
      if (!withinWriteLimit(ctx, session)) return;
      const parsed = await readJson(ctx, RECORD_BODY_LIMIT_BYTES, { optional: method === 'DELETE' });
      if (parsed === undefined) return;
      if (method !== 'DELETE') {
        if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed) || Object.keys(parsed).some((k) => k !== 'data')) {
          fail(ctx, 400, 'invalid_request', 'The body must be { "data": { ... } }.');
          return;
        }
        data = parsed.data;
      }
    }
    const guild = await managedGuild(ctx, session, guildId);
    if (!guild) return;

    const label = `${key}${itemId ? `/${itemId}` : ''}${actionId ? `!${actionId}` : ''}`;
    const what = `${method} ${label} in guild ${guildId}`;
    // The acting admin, for the stores' placeholders ({user.*}) and ping rules (owner / permissions of the OAuth entry).
    const user = {
      userId: session.uid,
      user: { id: session.uid, username: session.username, globalName: session.globalName, avatar: session.avatar },
      member: { owner: guild.owner === true, permissions: guild.permissions },
    };
    try {
      if (method === 'GET' && itemId === undefined) {
        const items = await records.list(guildId, key);
        json(ctx, 200, { items: Array.isArray(items) ? items : [], limit: collection.limit });
        return;
      }
      if (method === 'GET') {
        const item = await records.get(guildId, key, itemId);
        if (!item) fail(ctx, 404, 'not_found', `That ${collection.singular} does not exist any more.`);
        else json(ctx, 200, { item });
        return;
      }
      if (method === 'DELETE') {
        await records.remove(guildId, key, itemId, user);
        logger.info(`Dashboard: user ${session.uid} deleted ${label} in guild ${guildId}`);
        send(ctx, 204, undefined, { 'Cache-Control': 'no-store' });
        return;
      }

      let current = null;
      if (itemId !== undefined) {
        current = await records.get(guildId, key, itemId);
        if (!current) {
          fail(ctx, 404, 'not_found', `That ${collection.singular} does not exist any more.`);
          return;
        }
      }
      const mode = actionId !== undefined ? 'action' : itemId !== undefined ? 'update' : 'create';
      const valid = registry.validateRecordInput(key, data, { mode, actionId, current });
      const keep = current ? recordRefs(collection, current) : { channels: new Set(), roles: new Set() };
      if (!await verifyRefs(ctx, session, guildId, valid.refs, keep)) return;

      if (mode === 'create') {
        const item = await records.create(guildId, key, valid.data, user);
        logger.info(`Dashboard: user ${session.uid} created ${key}/${item?.id ?? '?'} in guild ${guildId}`);
        json(ctx, 201, { item });
      } else if (mode === 'update') {
        const item = await records.update(guildId, key, itemId, valid.data, user);
        logger.info(`Dashboard: user ${session.uid} changed ${label} in guild ${guildId}: ${Object.keys(valid.data).join(', ')}`);
        json(ctx, 200, { item });
      } else {
        const result = await records.action(guildId, key, itemId, actionId, valid.data, user);
        logger.info(`Dashboard: user ${session.uid} ran ${label} in guild ${guildId}`);
        json(ctx, 200, { item: result?.item ?? null, ...(typeof result?.message === 'string' ? { message: result.message } : {}) });
      }
    } catch (err) {
      recordFailure(ctx, err, { write, what });
    }
  }

  async function staticFile(ctx, [file, type]) {
    if (!allow(ctx, ['GET', 'HEAD'])) return;
    let body;
    try {
      body = await readFile(join(publicDir, file));
    } catch (err) {
      if (err?.code !== 'ENOENT') logger.warn(`Dashboard: could not read ${file}: ${err?.message ?? err}`);
      fail(ctx, 404, 'not_found', 'Not found.');
      return;
    }
    send(ctx, 200, body, { 'Content-Type': type, 'Cache-Control': 'no-cache' });
  }

  async function route(ctx) {
    const { path } = ctx;
    if (path === '/healthz') {
      if (allow(ctx, ['GET', 'HEAD'])) json(ctx, 200, { ok: true });
      return;
    }
    if (path === '/auth/login') return login(ctx);
    if (path === '/auth/callback') return callback(ctx);
    if (path === '/auth/logout') return logout(ctx);
    if (path === '/api/me') return me(ctx);
    if (path === '/api/catalog') return catalog(ctx);
    if (path === '/api/guilds') return guildList(ctx);
    const settingsMatch = /^\/api\/guilds\/([^/]{1,32})\/settings$/.exec(path);
    if (settingsMatch) return guildSettings(ctx, settingsMatch[1]);
    const resourcesMatch = /^\/api\/guilds\/([^/]{1,32})\/resources$/.exec(path);
    if (resourcesMatch) return guildResources(ctx, resourcesMatch[1]);
    const recordsMatch = /^\/api\/guilds\/([^/]{1,32})\/records\/([^/]{1,64})(?:\/([^/]{1,64})(?:\/actions\/([^/]{1,64}))?)?$/.exec(path);
    if (recordsMatch) return guildRecords(ctx, recordsMatch[1], recordsMatch[2], recordsMatch[3], recordsMatch[4]);
    const file = STATIC_FILES.get(path);
    if (file) return staticFile(ctx, file);
    return fail(ctx, 404, 'not_found', path.startsWith('/api/') ? 'Unknown API route.' : 'Not found.');
  }

  async function handle(req, res) {
    const ctx = { req, res, path: '/', query: new URLSearchParams(), cookieIn: new Map(), cookies: [] };
    try {
      const target = typeof req.url === 'string' ? req.url : '';
      if (!target.startsWith('/')) {
        fail(ctx, 400, 'invalid_request', 'Bad request target.');
        return;
      }
      const q = target.indexOf('?');
      ctx.path = q === -1 ? target : target.slice(0, q);
      ctx.query = new URLSearchParams(q === -1 ? '' : target.slice(q + 1));
      ctx.cookieIn = parseCookies(req.headers.cookie);
      await route(ctx);
      if (!res.headersSent) throw new Error('the handler did not answer');
    } catch (err) {
      logger.error(`Dashboard: ${forLog(req.method)} ${forLog(ctx.path)} failed`, err);
      if (res.headersSent) {
        res.destroy();
        return;
      }
      try {
        ctx.cookies = [];
        fail(ctx, 500, 'internal_error', 'Something went wrong.');
      } catch {
        res.destroy();
      }
    }
  }

  const server = createServer((req, res) => {
    handle(req, res).catch(() => res.destroy());
  });
  server.headersTimeout = 10_000;
  server.requestTimeout = 15_000;
  server.keepAliveTimeout = 5_000;
  return server;
}

/**
 * A stand-in server for a dashboard that is not configured: /healthz answers 200 (so a container stays
 * healthy and does not crash-loop) and everything else 503 naming the missing or invalid keys (names only).
 * @param {{ missing?: string[], invalid?: ({ key: string }|string)[] }} [opts]
 * @returns {import('node:http').Server}
 */
export function createParkedServer({ missing = [], invalid = [] } = {}) {
  const parts = [];
  if (missing.length > 0) parts.push(`missing ${missing.join(', ')}`);
  if (invalid.length > 0) parts.push(`invalid ${invalid.map((i) => (typeof i === 'string' ? i : i.key)).join(', ')}`);
  const message = `dashboard not configured: ${parts.join('; ') || 'unknown problem'}`;

  const server = createServer((req, res) => {
    const target = typeof req.url === 'string' ? req.url : '';
    const path = target.split('?')[0];
    const head = req.method === 'HEAD';
    let status = 503;
    let type = 'text/plain; charset=utf-8';
    let body = `${message}\n`;
    if (path === '/healthz') {
      status = 200;
      type = 'application/json; charset=utf-8';
      body = JSON.stringify({ ok: true, parked: true });
    } else if (path.startsWith('/api/')) {
      type = 'application/json; charset=utf-8';
      body = JSON.stringify({ error: 'not_configured', message });
    }
    res.writeHead(status, {
      ...SECURITY_HEADERS, 'Content-Type': type, 'Cache-Control': 'no-store', 'Content-Length': Buffer.byteLength(body),
    });
    res.end(head ? undefined : body);
  });
  server.headersTimeout = 5_000;
  server.requestTimeout = 10_000;
  server.keepAliveTimeout = 5_000;
  return server;
}
