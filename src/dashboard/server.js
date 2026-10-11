// The dashboard's HTTP server: JSON API, OAuth2 login, static UI files and security headers.
// createDashboardServer() returns a node:http Server that is not listening yet; dashboard.js (repo root)
// starts it. Settings and Discord access are injected through `deps`, so this file never opens Redis
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
import { buildCatalog, pickToggleValues, validateChanges } from './catalog.js';
import { DiscordApiError, guildIconUrl, userAvatarUrl } from './discordApi.js';

export const PUBLIC_DIR = fileURLToPath(new URL('./public/', import.meta.url));
export const BODY_LIMIT_BYTES = 16 * 1024;
export const PROPAGATION_SECONDS = 30;
export const WRITE_RATE_LIMIT = Object.freeze({ max: 20, windowMs: 10_000 });
const RATE_LIMIT_KEYS_MAX = 10_000;
const SNOWFLAKE_RE = /^\d{17,20}$/;
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
 *               writeToggles: (guildId: string, changes: Record<string, boolean>) => Promise<object> },
 *   now?: () => number,
 * }} opts.deps
 * @param {object} [opts.logger] info/warn/error, defaults to the bot's logger
 * @param {string} [opts.publicDir] directory of the static UI files
 * @returns {import('node:http').Server}
 */
export function createDashboardServer({ config, deps, logger = defaultLogger, publicDir = PUBLIC_DIR }) {
  const { discord, settings } = deps;
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
    if (ctx.req.method === 'GET') {
      const guild = await managedGuild(ctx, session, guildId);
      if (!guild) return;
      let row;
      try {
        row = await settings.readGuildSettings(guildId);
      } catch (err) {
        logger.warn(`Dashboard: could not load settings for guild ${guildId}: ${err?.message ?? err}`);
        fail(ctx, 503, 'settings_unavailable', 'The settings could not be loaded. Try again in a moment.');
        return;
      }
      json(ctx, 200, {
        guild: { id: guild.id, name: guild.name, iconUrl: guildIconUrl(guild) },
        values: pickToggleValues(row),
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
    const limited = limitWrites(session.uid);
    if (!limited.ok) {
      fail(ctx, 429, 'rate_limited', `Too many changes at once. Try again in ${limited.retryAfter} s.`,
        { retryAfter: limited.retryAfter }, { 'Retry-After': String(limited.retryAfter) });
      return;
    }
    const body = await readBody(ctx.req, BODY_LIMIT_BYTES);
    if (!body.ok) {
      if (body.status === 413) fail(ctx, 413, 'invalid_request', 'The request body is too large.', {}, { Connection: 'close' });
      else fail(ctx, 400, 'invalid_request', 'The request body could not be read.');
      return;
    }
    let parsed;
    try {
      parsed = JSON.parse(body.text);
    } catch {
      fail(ctx, 400, 'invalid_request', 'The request body is not valid JSON.');
      return;
    }
    const valid = validateChanges(parsed);
    if (!valid.ok) {
      fail(ctx, 400, 'invalid_request', valid.message);
      return;
    }
    const guild = await managedGuild(ctx, session, guildId);
    if (!guild) return;
    let row;
    try {
      row = await settings.writeToggles(guildId, valid.changes);
    } catch (err) {
      logger.error(`Dashboard: saving settings for guild ${guildId} failed`, err);
      fail(ctx, 500, 'save_failed', 'The change could not be saved. Try again in a moment.');
      return;
    }
    const summary = Object.entries(valid.changes).map(([field, value]) => `${field}=${value}`).join(', ');
    logger.info(`Dashboard: user ${session.uid} changed guild ${guildId}: ${summary}`);
    json(ctx, 200, { values: pickToggleValues(row) });
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
