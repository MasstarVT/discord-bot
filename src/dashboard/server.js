// The dashboard's HTTP server: JSON API (settings, server resources, per-server records), OAuth2 login, static UI
// files and security headers.
// createDashboardServer() returns a node:http Server that is not listening yet; dashboard.js (repo root)
// starts it. Settings, records and Discord access are injected through `deps`, so this file never opens Redis
// or Postgres connections itself.
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { gzip } from 'node:zlib';
import defaultLogger from '../utils/logger.js';
import {
  SESSION_COOKIE, STATE_COOKIE, SESSION_MAX_AGE_SECONDS, STATE_MAX_AGE_SECONDS,
  buildAuthorizeUrl, canManageGuild, checkWriteRequest, clearCookie, createOAuthState, deriveSessionKey,
  parseCookies, sealSession, serializeCookie, statesMatch, unsealSession,
} from './auth.js';
import { buildCatalog } from './catalog.js';
import { renderChangelog, renderDocs, renderLanding, renderLegal } from './landing.js';
import { normalizeIp } from './config.js';
import { DiscordApiError, guildIconUrl, userAvatarUrl } from './discordApi.js';
import { FieldError, checkRefs, collectAssignableRoles, collectStoredRefs, refKey, storedRefKeys } from './schema.js';
import { collectSettingsRefKeys, collectSettingsRefs, readSettingsValues, validateSettingsChanges } from './fields.js';
import { COLLECTIONS, RECORD_ID_RE, validateRecordInput } from './collections/index.js';
import { RecordError, unwrapResult } from './collections/errors.js';

export const PUBLIC_DIR = fileURLToPath(new URL('./public/', import.meta.url));
/** Body limit of a settings PATCH (a full page of messages and embeds fits). */
export const BODY_LIMIT_BYTES = 256 * 1024;
/** Body limit of a record write. */
export const RECORD_BODY_LIMIT_BYTES = 128 * 1024;
export const PROPAGATION_SECONDS = 30;
export const WRITE_RATE_LIMIT = Object.freeze({ max: 20, windowMs: 10_000 });
/** Member searches (user pickers search as the admin types). */
export const SEARCH_RATE_LIMIT = Object.freeze({ max: 30, windowMs: 10_000 });
/**
 * Login callbacks that get as far as Discord's token endpoint (a matching state and a well-formed code; every one costs a
 * POST with the app credentials): at most 10 per client address and 60 for the whole dashboard per minute. A real login
 * is one request, so this only bites a flood of junk that would otherwise get the app or the address rate limited.
 */
export const LOGIN_RATE_LIMIT = Object.freeze({ max: 10, windowMs: 60_000 });
export const LOGIN_GLOBAL_RATE_LIMIT = Object.freeze({ max: 60, windowMs: 60_000 });
const RATE_LIMIT_KEYS_MAX = 10_000;
const LOOPBACK_ADDRESSES = new Set(['127.0.0.1', '::1', '::ffff:127.0.0.1']);
const SNOWFLAKE_RE = /^\d{17,20}$/;
const ACTION_ID_RE = /^[A-Za-z][A-Za-z0-9_-]{0,39}$/;
/** Channels tried for a bot-owner invite before giving up. */
const OWNER_INVITE_ATTEMPTS = 5;
/** Where the dashboard app lives (the landing page has `/`). */
const DASHBOARD_PATH = '/dashboard';
const LOGIN_FAILED = `${DASHBOARD_PATH}?error=login_failed`;
const STATE_PATH = '/auth';

/** The only files served from PUBLIC_DIR: request path -> [file name, content type]. */
const STATIC_FILES = new Map([
  ['/', ['landing.html', 'text/html; charset=utf-8']],
  [DASHBOARD_PATH, ['index.html', 'text/html; charset=utf-8']],
  ['/index.html', ['index.html', 'text/html; charset=utf-8']],
  ['/landing.js', ['landing.js', 'text/javascript; charset=utf-8']],
  ['/app.js', ['app.js', 'text/javascript; charset=utf-8']],
  ['/lib.js', ['lib.js', 'text/javascript; charset=utf-8']],
  ['/forms.js', ['forms.js', 'text/javascript; charset=utf-8']],
  ['/collections.js', ['collections.js', 'text/javascript; charset=utf-8']],
  ['/embed-editor.js', ['embed-editor.js', 'text/javascript; charset=utf-8']],
  ['/tokens.css', ['tokens.css', 'text/css; charset=utf-8']],
  ['/styles.css', ['styles.css', 'text/css; charset=utf-8']],
  ['/landing.css', ['landing.css', 'text/css; charset=utf-8']],
  ['/site.css', ['site.css', 'text/css; charset=utf-8']],
  ['/leaflet.css', ['leaflet.css', 'text/css; charset=utf-8']],
  ['/docs', ['docs.html', 'text/html; charset=utf-8']],
  ['/changelog', ['changelog.html', 'text/html; charset=utf-8']],
  ['/demo/rank-card.png', ['demo/rank-card.png', 'image/png']],
  ['/demo/welcome-card.png', ['demo/welcome-card.png', 'image/png']],
  ['/forms.css', ['forms.css', 'text/css; charset=utf-8']],
  ['/collections.css', ['collections.css', 'text/css; charset=utf-8']],
  ['/favicon.svg', ['favicon.svg', 'image/svg+xml']],
  ['/privacy', ['privacy.html', 'text/html; charset=utf-8']],
  ['/terms', ['terms.html', 'text/html; charset=utf-8']],
]);

/**
 * Pages whose `<!--contact-->` marker is replaced with DASHBOARD_CONTACT when they are loaded. They wear the public
 * pages' header and footer (renderLegal).
 */
const LEGAL_PAGES = new Set(['privacy.html', 'terms.html']);
const CONTACT_MARKER = '<!--contact-->';
/** The public pages and their renderers (src/dashboard/landing.js). */
const PUBLIC_PAGES = new Map([['landing.html', renderLanding], ['docs.html', renderDocs], ['changelog.html', renderChangelog]]);
const NO_CONTACT = 'the bot owner, through the server where you use the bot';

/** CHANGELOG.md at the repo root: the /changelog page and the landing page's newest entries. */
const CHANGELOG_PATH = fileURLToPath(new URL('../../CHANGELOG.md', import.meta.url));

/** The changelog's text, or '' when the file is missing (the pages then list no entries). */
function readChangelog() {
  try {
    return readFileSync(CHANGELOG_PATH, 'utf8');
  } catch {
    return '';
  }
}

const escapeHtml = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

/** The HTML a legal page shows for the configured contact (config.contact), escaped. Exported for tests. */
export function contactHtml(contact) {
  if (!contact) return escapeHtml(NO_CONTACT);
  const href = contact.kind === 'email' ? `mailto:${contact.value}` : contact.value;
  return `<a href="${escapeHtml(href)}">${escapeHtml(contact.value)}</a>`;
}

const gzipAsync = promisify(gzip);
const COMPRESSIBLE_RE = /^(?:text\/|image\/svg\+xml)/;

/** One static file in memory: its bytes, a gzip copy when that is smaller, and a strong ETag per encoding. */
async function loadStaticEntry(path, type, transform) {
  const body = transform ? Buffer.from(transform((await readFile(path)).toString('utf8')), 'utf8') : await readFile(path);
  const hash = createHash('sha256').update(body).digest('base64url').slice(0, 27);
  let zipped = null;
  if (COMPRESSIBLE_RE.test(type)) {
    const packed = await gzipAsync(body, { level: 9 });
    if (packed.length < body.length) zipped = packed;
  }
  return { body, etag: `"${hash}"`, gzip: zipped, gzipEtag: `"${hash}-gz"` };
}

/** True when an Accept-Encoding header allows gzip (`gzip;q=0` refuses it). Exported for tests. */
export function acceptsGzip(header) {
  for (const part of String(header ?? '').toLowerCase().split(',')) {
    const [name, ...params] = part.split(';').map((x) => x.trim());
    if (name !== 'gzip') continue;
    const q = params.find((x) => x.startsWith('q='));
    return !q || Number(q.slice(2)) > 0;
  }
  return false;
}

/** If-None-Match against the current ETags: `*`, a list, and weak (W/) tags all count (RFC 9110 weak comparison). */
export function etagMatches(header, etags) {
  if (typeof header !== 'string' || !header.trim()) return false;
  if (header.trim() === '*') return true;
  return header.split(',').some((tag) => etags.includes(tag.trim().replace(/^W\//, '')));
}

export const SECURITY_HEADERS = Object.freeze({
  'Content-Security-Policy': "default-src 'self'; img-src 'self' https://cdn.discordapp.com data:; style-src 'self' https://fonts.googleapis.com; "
    + "font-src https://fonts.gstatic.com; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
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

/**
 * The client address the login limit counts. A request is counted under its socket address; only when that is the
 * loopback address (a reverse proxy on the same host) or one of DASHBOARD_TRUSTED_PROXIES (a proxy on another host, such
 * as a Cloudflare Tunnel connector) is the last `X-Forwarded-For` entry, the one that proxy appended, used instead. From
 * any other peer the header is ignored, so a client cannot pick its own key.
 * @param {import('node:http').IncomingMessage} req
 * @param {ReadonlySet<string>} [trusted] normalised addresses of the trusted proxies (config.trustedProxies)
 * @returns {string}
 */
export function clientAddress(req, trusted = new Set()) {
  const peer = req.socket?.remoteAddress ?? 'unknown';
  if (!LOOPBACK_ADDRESSES.has(peer) && !trusted.has(normalizeIp(peer))) return peer;
  const header = req.headers?.['x-forwarded-for'];
  const last = typeof header === 'string' ? header.split(',').at(-1).trim() : '';
  return last !== '' && last.length <= 64 ? last : peer;
}

const formatColor = (n) => (Number.isInteger(n) && n >= 0 && n <= 0xffffff
  ? `#${n.toString(16).padStart(6, '0').toUpperCase()}`
  : null);

/** RecordError code -> the `error` of the answer. */
/** A records list's ?query= (searchable collections) and ?cursor= (the next page). */
const LIST_QUERY_MAX = 100;
const LIST_CURSOR_RE = /^[A-Za-z0-9_:.-]{1,200}$/;

const RECORD_ERROR_CODES = Object.freeze({
  invalid: 'invalid_request', not_found: 'not_found', conflict: 'conflict', limit: 'limit', discord: 'discord_failed',
  failed: 'action_failed', unavailable: 'unavailable',
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
  const { discord, settings, records = null, botJoins = null } = deps;
  const registry = deps.collections ?? { COLLECTIONS, validateRecordInput };
  const now = deps.now ?? Date.now;
  const sessionKey = deriveSessionKey(config.sessionSecret);
  const secure = config.secureCookies === true;
  const baseHeaders = secure
    ? { ...SECURITY_HEADERS, 'Strict-Transport-Security': 'max-age=31536000' }
    : { ...SECURITY_HEADERS };
  const catalogJson = JSON.stringify(buildCatalog());
  const limitWrites = createRateLimiter({ ...WRITE_RATE_LIMIT, now });
  const limitSearches = createRateLimiter({ ...SEARCH_RATE_LIMIT, now });
  const limitLoginsByAddress = createRateLimiter({ ...LOGIN_RATE_LIMIT, now });
  const limitLoginsOverall = createRateLimiter({ ...LOGIN_GLOBAL_RATE_LIMIT, now });
  const trustedProxies = new Set((config.trustedProxies ?? []).map(normalizeIp));

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

  /** BOT_OWNER_ID may manage every server the bot is in, member or not (the owner part of the server list). */
  const isBotOwner = (session) => Boolean(config.botOwnerId) && session.uid === config.botOwnerId;

  /** The bot's own entry for a server ({ id, name, icon }), or null when Discord cannot say. */
  async function botGuildEntry(guildId) {
    try {
      return (await discord.getBotGuilds()).find((g) => g.id === guildId) ?? null;
    } catch {
      return null;
    }
  }

  /**
   * The guild entry the routes use when the user may manage the guild and the bot is in it; otherwise answers and
   * returns null. A user needs the guild in their OAuth list as owner, Administrator or Manage Server. The bot owner
   * may open any server the bot is in and acts there as its owner (`owner: true` for the ping and role rules).
   */
  async function managedGuild(ctx, session, guildId) {
    const guilds = await userGuilds(ctx, session);
    if (!guilds) return null;
    const guild = guilds.find((g) => g.id === guildId);
    const botOwner = isBotOwner(session);
    // Checked before the bot's presence, so other users cannot probe which servers the bot is in.
    if (!botOwner && !canManageGuild(guild)) {
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
    if (!botOwner) return guild;
    const listed = guild ?? await botGuildEntry(guildId);
    return {
      id: guildId,
      name: typeof listed?.name === 'string' ? listed.name : 'Unknown server',
      icon: listed?.icon ?? null,
      owner: true,
      permissions: guild?.permissions ?? '0',
    };
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

    // From here on the request costs a POST to Discord, so it is counted (per address first: a flood from one address
    // must not use up the overall allowance of everyone else).
    const address = clientAddress(ctx.req, trustedProxies);
    const limited = limitLoginsByAddress(address);
    const limitedOverall = limited.ok ? limitLoginsOverall('all') : limited;
    if (!limitedOverall.ok) {
      logger.warn(`Dashboard login rate limited: ${limited.ok ? 'overall limit' : forLog(address)}`);
      return fail(ctx, 429, 'rate_limited', `Too many login attempts. Try again in ${limitedOverall.retryAfter} s.`,
        { retryAfter: limitedOverall.retryAfter }, { 'Retry-After': String(limitedOverall.retryAfter) });
    }

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
    return redirect(ctx, DASHBOARD_PATH);
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
    const manageable = guilds.filter((g) => canManageGuild(g));
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
    if (!isBotOwner(session)) {
      json(ctx, 200, { guilds: list });
      return;
    }
    const memberOf = new Set(guilds.map((g) => g.id));
    const ownerGuilds = await ownerGuildList(new Set(list.map((g) => g.id)), memberOf);
    await addInviters([...list, ...ownerGuilds]);
    json(ctx, 200, { guilds: list, ownerGuilds });
  }

  /**
   * For the bot owner: sets `addedBy` on each server the bot is in, from the BotJoin rows the bot records: null when
   * nothing is recorded yet, else `{ userId, name, source, joinedAt }` (userId null: the bot could not tell who). A
   * database failure, or no `botJoins` dependency, leaves the field out.
   */
  async function addInviters(entries) {
    if (!botJoins) return;
    const present = entries.filter((g) => g.botPresent);
    let rows;
    try {
      rows = await botJoins.listBotJoins(present.map((g) => g.id));
    } catch (err) {
      logger.warn(`Dashboard: could not read who added the bot: ${err?.message ?? err}`);
      return;
    }
    for (const g of present) {
      const row = rows.get(g.id);
      g.addedBy = row
        ? { userId: row.inviterId, name: row.inviterName, source: row.source, joinedAt: row.joinedAt.toISOString() }
        : null;
    }
  }

  /**
   * For the bot owner: every server the bot is in that is not in their own list, with `member` (whether they are in
   * it, from their whole OAuth list). A Discord failure leaves it empty.
   */
  async function ownerGuildList(skip, memberOf) {
    let all;
    try {
      all = await discord.getBotGuilds();
    } catch (err) {
      logger.warn(`Dashboard: could not list the bot's servers for the bot owner: ${err?.message ?? err}`);
      return [];
    }
    return all
      .filter((g) => !skip.has(g.id))
      .map((g) => ({
        id: g.id, name: g.name, iconUrl: guildIconUrl(g), botPresent: true, inviteUrl: null, ownerAccess: true, member: memberOf.has(g.id),
      }))
      .sort((a, b) => nameCollator.compare(a.name, b.name) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  }

  /**
   * POST /api/guilds/:id/invite: a single-use, 30-minute invite the bot creates so the bot owner can join a server it
   * is in. Bot owner only, refused before anything shows whether the bot is there. Tries a few text, announcement,
   * then voice channels: one where the bot lacks Create Invite is skipped, any other Discord error stops.
   */
  async function ownerInvite(ctx, guildId) {
    if (!allow(ctx, ['POST'])) return;
    const check = checkWriteRequest(ctx.req, config);
    if (!check.ok) {
      fail(ctx, check.status, check.error, check.message);
      return;
    }
    const session = requireSession(ctx);
    if (!session) return;
    if (!SNOWFLAKE_RE.test(guildId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid server id.');
      return;
    }
    if (!isBotOwner(session)) {
      fail(ctx, 403, 'forbidden', 'Only the bot owner can create an invite here.');
      return;
    }
    if (!withinWriteLimit(ctx, session)) return;
    if (!(await managedGuild(ctx, session, guildId))) return;
    let res;
    try {
      res = await discord.getGuildResources(guildId);
    } catch (err) {
      discordFailure(ctx, err, false);
      return;
    }
    if (!res.bot.createInvite) {
      fail(ctx, 422, 'action_failed', 'The bot needs the Create Invite permission in this server to invite you.');
      return;
    }
    const byKind = (...kinds) => res.channels.filter((c) => kinds.includes(c.kind));
    const candidates = [...byKind('text', 'announcement'), ...byKind('voice', 'stage')].slice(0, OWNER_INVITE_ATTEMPTS);
    for (const channel of candidates) {
      try {
        const invite = await discord.createInvite(channel.id, { reason: 'Requested by the bot owner from the dashboard' });
        logger.info(`Dashboard: the bot owner created an invite for guild ${guildId} (channel ${channel.id})`);
        json(ctx, 200, { url: invite.url, expiresAt: invite.expiresAt, channel: { id: channel.id, name: channel.name } });
        return;
      } catch (err) {
        // 50013 Missing Permissions / 50001 Missing Access: a channel override; try the next one.
        if (err instanceof DiscordApiError && (err.discordCode === 50013 || err.discordCode === 50001)) continue;
        discordFailure(ctx, err, false);
        return;
      }
    }
    fail(ctx, 422, 'action_failed', 'The bot could not create an invite in the channels it tried. Let it Create Invite in a text channel.');
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
   * (checkAssignableRole). Only a role already stored in the same field skips the giving checks: moved into a field
   * that hands it out (e.g. from the mod roles to the autoroles), it is checked like a new one. Answers and returns
   * false on a problem.
   * @param {import('./schema.js').Refs} refs
   * @param {{ channels: Set<string>, roles: Set<string>, keys: Set<string> }} keep stored ids, and their refKeys
   */
  async function verifyRefs(ctx, session, guildId, refs, keep, keyOf = (r) => refKey(r.path, r.id)) {
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

    const given = refs.roles.filter((r) => r.assignable && !keep.keys.has(keyOf(r)) && map.roles.has(r.id));
    if (given.length === 0) return true;
    const problem = (ref, message) => {
      fail(ctx, 400, 'invalid_request', message, { path: ref.path });
      return false;
    };
    if (!res.bot.manageRoles) return problem(given[0], 'The bot needs the Manage Roles permission to give roles.');
    for (const ref of given) {
      // Level rewards and achievement role removals may use such roles, as /levelconfig and /achievement-config allow.
      if (map.roles.get(ref.id).dangerous && !ref.allowDangerous) {
        return problem(ref, 'That role has moderator or admin permissions (such as Administrator, Manage Server, Ban or Kick Members). '
          + 'The bot does not hand those out; pick a role without them.');
      }
    }
    if (res.guild.ownerId === session.uid || isBotOwner(session)) return true;
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

  /**
   * For a collection that opts in (`checkStoredRoles`): a record that gives or takes a role at or above the user's highest role
   * cannot be changed by that user, whatever the change is (verifyRefs only looks at the ids a request sets, so enabling or
   * editing a stored record would pass unchecked). Looks at every assignable role of the record as it will be saved; the guild
   * owner and the bot owner are exempt. Answers and returns false on a problem.
   * @param {object} saved the record as it will be stored (stored fields merged with the request)
   * @param {object} changed the fields the request sets
   */
  async function verifyStoredRoles(ctx, session, guildId, collection, saved, changed) {
    const held = collectAssignableRoles(collection.fields, saved);
    if (held.length === 0) return true;
    let res;
    let member;
    try {
      res = await discord.getGuildResources(guildId);
      if (res.guild.ownerId === session.uid || isBotOwner(session)) return true;
      member = await discord.getMember(guildId, session.uid);
    } catch (err) {
      discordFailure(ctx, err, false);
      return false;
    }
    const map = refResources(res);
    const top = Math.max(0, ...(member?.roles ?? []).map((id) => map.roles.get(id)?.position ?? 0));
    const above = held.find((ref) => (map.roles.get(ref.id)?.position ?? -1) >= top);
    if (!above) return true;
    const field = above.path.split('.')[0];
    fail(ctx, 400, 'invalid_request',
      `This ${collection.singular} gives or takes a role that is not below your highest role, so you cannot change it.`,
      Object.hasOwn(changed ?? {}, field) ? { path: above.path } : {});
    return false;
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
      const keep = { ...collectSettingsRefs(loaded.row), keys: collectSettingsRefKeys(loaded.row) };
      if (!await verifyRefs(ctx, session, guildId, valid.refs, keep)) return;
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

  /** GET /api/guilds/:id/members?query=: member search for the user pickers (Discord's search, or one id). */
  async function guildMembers(ctx, guildId) {
    if (!allow(ctx, ['GET'])) return;
    const session = requireSession(ctx);
    if (!session) return;
    if (!SNOWFLAKE_RE.test(guildId)) {
      fail(ctx, 400, 'invalid_request', 'That is not a valid server id.');
      return;
    }
    const query = (ctx.query.get('query') ?? '').replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, LIST_QUERY_MAX);
    if (!query) {
      json(ctx, 200, { members: [] });
      return;
    }
    const limited = limitSearches(session.uid);
    if (!limited.ok) {
      fail(ctx, 429, 'rate_limited', `Too many searches at once. Try again in ${limited.retryAfter} s.`,
        { retryAfter: limited.retryAfter }, { 'Retry-After': String(limited.retryAfter) });
      return;
    }
    const guild = await managedGuild(ctx, session, guildId);
    if (!guild) return;
    let members;
    try {
      members = await discord.searchMembers(guildId, query);
    } catch (err) {
      discordFailure(ctx, err, false);
      return;
    }
    json(ctx, 200, { members });
  }

  /** Answers a failed record operation. */
  function recordFailure(ctx, err, { write, what }) {
    if (err instanceof FieldError) return fail(ctx, 400, 'invalid_request', err.message, err.path ? { path: err.path } : {});
    if (err instanceof RecordError) {
      if (err.code === 'discord') logger.warn(`Dashboard: ${what}: ${err.message}`);
      return fail(ctx, err.status, RECORD_ERROR_CODES[err.code], err.message,
        { ...(err.path ? { path: err.path } : {}), ...(err.saved ? { saved: true } : {}) });
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

  /** Every channel and role id a stored record holds (checkRefs' keep sets), and which field holds it (keys). */
  function recordRefs(collection, item) {
    const out = { channels: new Set(), roles: new Set(), keys: new Set() };
    for (const desc of [...collection.fields, ...(collection.readOnly ?? [])]) {
      collectStoredRefs(desc, item?.[desc.field], out);
      storedRefKeys(desc, item?.[desc.field], desc.field, out.keys);
    }
    // A collection whose field means something else per item type (an automation action's roles are given or taken)
    // refines the keys, so a role moved from one meaning to the other is checked like a new one.
    if (typeof collection.refKeys === 'function') out.keys = collection.refKeys(item, out.keys);
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
        // Searchable collections take ?query= (what to look for) and ?cursor= (the page after a previous answer).
        const query = (ctx.query.get('query') ?? '').replace(/[\u0000-\u001f\u007f]/g, '').trim().slice(0, LIST_QUERY_MAX);
        // A search may ask Discord (member names), so it shares the member search's per-user limit.
        if (query) {
          const limited = limitSearches(session.uid);
          if (!limited.ok) {
            fail(ctx, 429, 'rate_limited', `Too many searches at once. Try again in ${limited.retryAfter} s.`,
              { retryAfter: limited.retryAfter }, { 'Retry-After': String(limited.retryAfter) });
            return;
          }
        }
        const rawCursor = ctx.query.get('cursor') ?? '';
        const cursor = LIST_CURSOR_RE.test(rawCursor) ? rawCursor : '';
        const result = await records.list(guildId, key, { query, cursor });
        const items = Array.isArray(result) ? result : Array.isArray(result?.items) ? result.items : [];
        const nextCursor = !Array.isArray(result) && typeof result?.nextCursor === 'string' && LIST_CURSOR_RE.test(result.nextCursor)
          ? result.nextCursor : null;
        json(ctx, 200, { items, limit: collection.limit, ...(nextCursor ? { nextCursor } : {}) });
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
      const keep = current ? recordRefs(collection, current) : { channels: new Set(), roles: new Set(), keys: new Set() };
      const keyOf = typeof collection.refKeyOf === 'function'
        ? (r) => collection.refKeyOf(r, valid.data)
        : (r) => refKey(r.path, r.id);
      if (!await verifyRefs(ctx, session, guildId, valid.refs, keep, keyOf)) return;
      if (mode === 'update' && collection.checkStoredRoles === true
        && !await verifyStoredRoles(ctx, session, guildId, collection, { ...current, ...valid.data }, valid.data)) return;

      if (mode === 'create') {
        const { item, message } = unwrapResult(await records.create(guildId, key, valid.data, user));
        logger.info(`Dashboard: user ${session.uid} created ${key}/${item?.id ?? '?'} in guild ${guildId}`);
        json(ctx, 201, { item, ...(message ? { message } : {}) });
      } else if (mode === 'update') {
        const { item, message } = unwrapResult(await records.update(guildId, key, itemId, valid.data, user));
        logger.info(`Dashboard: user ${session.uid} changed ${label} in guild ${guildId}: ${Object.keys(valid.data).join(', ')}`);
        json(ctx, 200, { item, ...(message ? { message } : {}) });
      } else {
        const result = await records.action(guildId, key, itemId, actionId, valid.data, user);
        logger.info(`Dashboard: user ${session.uid} ran ${label} in guild ${guildId}`);
        json(ctx, 200, { item: result?.item ?? null, ...(typeof result?.message === 'string' ? { message: result.message } : {}) });
      }
    } catch (err) {
      recordFailure(ctx, err, { write, what });
    }
  }

  // Static files are read once, on first request (restart the dashboard after editing them), so a page load is served
  // from memory, gzip'd once, and a revalidation (Cache-Control: no-cache) answers 304 without a body.
  const staticEntries = new Map();
  function staticEntry(file, type) {
    let entry = staticEntries.get(file);
    if (!entry) {
      let transform;
      if (LEGAL_PAGES.has(file)) {
        // The legal text names the contact itself (contactHtml), so the footer leaves out its "Contact" link.
        const options = { inviteUrl: discord.inviteUrl(), supportUrl: config.supportUrl };
        transform = (text) => renderLegal(text.replaceAll(CONTACT_MARKER, contactHtml(config.contact)), options);
      } else if (PUBLIC_PAGES.has(file)) {
        // The public pages are filled from the catalog, the command list, CHANGELOG.md and the bot's client id.
        const options = { inviteUrl: discord.inviteUrl(), contact: config.contact, supportUrl: config.supportUrl, changelog: readChangelog() };
        transform = (text) => PUBLIC_PAGES.get(file)(text, options);
      }
      entry = loadStaticEntry(join(publicDir, file), type, transform);
      staticEntries.set(file, entry);
      entry.catch(() => staticEntries.delete(file)); // missing or unreadable: look again next time
    }
    return entry;
  }

  async function staticFile(ctx, [file, type]) {
    if (!allow(ctx, ['GET', 'HEAD'])) return;
    let entry;
    try {
      entry = await staticEntry(file, type);
    } catch (err) {
      if (err?.code !== 'ENOENT') logger.warn(`Dashboard: could not read ${file}: ${err?.message ?? err}`);
      fail(ctx, 404, 'not_found', 'Not found.');
      return;
    }
    const zipped = entry.gzip !== null && acceptsGzip(ctx.req.headers['accept-encoding']);
    const headers = { 'Cache-Control': 'no-cache', ETag: zipped ? entry.gzipEtag : entry.etag };
    if (entry.gzip) headers.Vary = 'Accept-Encoding';
    if (etagMatches(ctx.req.headers['if-none-match'], [entry.etag, entry.gzipEtag])) {
      send(ctx, 304, undefined, headers);
      return;
    }
    if (zipped) headers['Content-Encoding'] = 'gzip';
    send(ctx, 200, zipped ? entry.gzip : entry.body, { 'Content-Type': type, ...headers });
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
    const membersMatch = /^\/api\/guilds\/([^/]{1,32})\/members$/.exec(path);
    if (membersMatch) return guildMembers(ctx, membersMatch[1]);
    const inviteMatch = /^\/api\/guilds\/([^/]{1,32})\/invite$/.exec(path);
    if (inviteMatch) return ownerInvite(ctx, inviteMatch[1]);
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
