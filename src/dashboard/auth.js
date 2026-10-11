// Dashboard authentication: Discord OAuth2 (authorization code), the encrypted session cookie, the OAuth
// `state` cookie and the CSRF checks for writes. Pure helpers plus two outbound calls; nothing runs at import.
import { createCipheriv, createDecipheriv, hkdfSync, randomBytes, timingSafeEqual } from 'node:crypto';
import { OAuth2Routes } from 'discord.js';

export const SESSION_COOKIE = 'dash_session';
export const STATE_COOKIE = 'dash_oauth_state';
export const SESSION_MAX_AGE_SECONDS = 7 * 24 * 60 * 60;
export const STATE_MAX_AGE_SECONDS = 10 * 60;
export const OAUTH_SCOPES = Object.freeze(['identify', 'guilds']);
export const OAUTH_TIMEOUT_MS = 10_000;

const SEAL_VERSION = 'v1';
const IV_BYTES = 12;
const TAG_BYTES = 16;
const MAX_SEALED_LENGTH = 4096;
const HKDF_SALT = 'discord-bot-dashboard';
const HKDF_INFO = `session-cookie/${SEAL_VERSION}`;
const ADMINISTRATOR = 0x8n;
const MANAGE_GUILD = 0x20n;

// ── Session sealing ───────────────────────────────────────────────────────────

/**
 * Derives the 32-byte AES key for session cookies from DASHBOARD_SESSION_SECRET (HKDF-SHA256).
 * @param {string} secret
 * @returns {Buffer}
 */
export function deriveSessionKey(secret) {
  if (typeof secret !== 'string' || secret.length === 0) throw new TypeError('deriveSessionKey needs a secret');
  return Buffer.from(hkdfSync('sha256', secret, HKDF_SALT, HKDF_INFO, 32));
}

/**
 * Encrypts and authenticates a JSON payload (AES-256-GCM, random 12-byte IV) into `v1.<base64url>`.
 * @param {object} payload must carry a numeric `exp` (ms since epoch)
 * @param {Buffer} key from deriveSessionKey
 * @returns {string}
 */
export function sealSession(payload, key) {
  const iv = randomBytes(IV_BYTES);
  const cipher = createCipheriv('aes-256-gcm', key, iv);
  cipher.setAAD(Buffer.from(SEAL_VERSION));
  const body = Buffer.concat([cipher.update(JSON.stringify(payload), 'utf8'), cipher.final()]);
  return `${SEAL_VERSION}.${Buffer.concat([iv, body, cipher.getAuthTag()]).toString('base64url')}`;
}

/**
 * Reverses sealSession. Never throws: a wrong key, a tampered or truncated value, an unknown version,
 * malformed JSON or an `exp` at or before `now` all return null.
 * @param {unknown} token
 * @param {Buffer} key
 * @param {number} [now] ms since epoch
 * @returns {object|null}
 */
export function unsealSession(token, key, now = Date.now()) {
  try {
    if (typeof token !== 'string' || token.length > MAX_SEALED_LENGTH) return null;
    const prefix = `${SEAL_VERSION}.`;
    if (!token.startsWith(prefix)) return null;
    const encoded = token.slice(prefix.length);
    if (!/^[A-Za-z0-9_-]+$/.test(encoded)) return null;
    const raw = Buffer.from(encoded, 'base64url');
    if (raw.length < IV_BYTES + TAG_BYTES + 2) return null;
    const iv = raw.subarray(0, IV_BYTES);
    const tag = raw.subarray(raw.length - TAG_BYTES);
    const body = raw.subarray(IV_BYTES, raw.length - TAG_BYTES);
    const decipher = createDecipheriv('aes-256-gcm', key, iv);
    decipher.setAAD(Buffer.from(SEAL_VERSION));
    decipher.setAuthTag(tag);
    const json = Buffer.concat([decipher.update(body), decipher.final()]).toString('utf8');
    const payload = JSON.parse(json);
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) return null;
    if (typeof payload.exp !== 'number' || !Number.isFinite(payload.exp) || payload.exp <= now) return null;
    return payload;
  } catch {
    return null;
  }
}

// ── Cookies ───────────────────────────────────────────────────────────────────

/**
 * Parses a Cookie header. The first occurrence of a name wins; malformed pairs are skipped.
 * @param {string|undefined} header
 * @returns {Map<string, string>}
 */
export function parseCookies(header) {
  const out = new Map();
  if (typeof header !== 'string' || header.length === 0) return out;
  for (const part of header.split(';')) {
    const eq = part.indexOf('=');
    if (eq <= 0) continue;
    const name = part.slice(0, eq).trim();
    if (!name || out.has(name)) continue;
    let value = part.slice(eq + 1).trim();
    if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) value = value.slice(1, -1);
    try {
      out.set(name, decodeURIComponent(value));
    } catch {
      // Not valid percent-encoding: ignore this cookie.
    }
  }
  return out;
}

/**
 * Builds a Set-Cookie value. HttpOnly and SameSite=Lax are always set.
 * @param {string} name
 * @param {string} value
 * @param {{ maxAge?: number, path?: string, secure?: boolean }} [opts] maxAge in seconds
 * @returns {string}
 */
export function serializeCookie(name, value, { maxAge, path = '/', secure = false } = {}) {
  const parts = [`${name}=${encodeURIComponent(value)}`, `Path=${path}`, 'HttpOnly', 'SameSite=Lax'];
  if (maxAge !== undefined) {
    const seconds = Math.max(0, Math.floor(maxAge));
    parts.push(`Max-Age=${seconds}`);
    if (seconds === 0) parts.push('Expires=Thu, 01 Jan 1970 00:00:00 GMT');
  }
  if (secure) parts.push('Secure');
  return parts.join('; ');
}

/**
 * A Set-Cookie value that deletes a cookie.
 * @param {string} name
 * @param {{ path?: string, secure?: boolean }} [opts]
 */
export function clearCookie(name, { path = '/', secure = false } = {}) {
  return serializeCookie(name, '', { maxAge: 0, path, secure });
}

// ── OAuth2 ────────────────────────────────────────────────────────────────────

/** @returns {string} a fresh random OAuth `state` (32 bytes, base64url) */
export function createOAuthState() {
  return randomBytes(32).toString('base64url');
}

/**
 * Constant-time comparison of the `state` query parameter with the cookie value.
 * @param {unknown} expected cookie value
 * @param {unknown} actual query value
 * @returns {boolean}
 */
export function statesMatch(expected, actual) {
  if (typeof expected !== 'string' || typeof actual !== 'string' || expected.length === 0) return false;
  const a = Buffer.from(expected);
  const b = Buffer.from(actual);
  return a.length === b.length && timingSafeEqual(a, b);
}

/**
 * The Discord authorize URL for a login (scopes `identify guilds`, consent skipped when already given).
 * @param {{ clientId: string, redirectUri: string }} config
 * @param {string} state
 * @returns {string}
 */
export function buildAuthorizeUrl(config, state) {
  const url = new URL('https://discord.com/oauth2/authorize');
  url.search = new URLSearchParams({
    response_type: 'code',
    client_id: config.clientId,
    scope: OAUTH_SCOPES.join(' '),
    redirect_uri: config.redirectUri,
    state,
    prompt: 'none',
  }).toString();
  return url.toString();
}

const basicAuth = (config) => `Basic ${Buffer.from(
  `${encodeURIComponent(config.clientId)}:${encodeURIComponent(config.clientSecret)}`,
).toString('base64')}`;

async function postForm(fetchImpl, url, config, form) {
  return fetchImpl(url, {
    method: 'POST',
    headers: {
      Authorization: basicAuth(config),
      'Content-Type': 'application/x-www-form-urlencoded',
      Accept: 'application/json',
    },
    body: new URLSearchParams(form).toString(),
    signal: AbortSignal.timeout(OAUTH_TIMEOUT_MS),
    redirect: 'error',
  });
}

/**
 * Exchanges an authorization code for an access token.
 * @param {{ config: object, fetchImpl?: typeof fetch }} ctx config: clientId, clientSecret, redirectUri
 * @param {string} code
 * @returns {Promise<{ accessToken: string, expiresIn: number }>} expiresIn in seconds
 * @throws Error (with `status` for HTTP failures) on any failure; the message never contains a token
 */
export async function exchangeCode({ config, fetchImpl = fetch }, code) {
  const res = await postForm(fetchImpl, OAuth2Routes.tokenURL, config, {
    grant_type: 'authorization_code',
    code,
    redirect_uri: config.redirectUri,
  });
  if (!res.ok) {
    await res.body?.cancel?.().catch(() => {});
    throw Object.assign(new Error(`token exchange returned HTTP ${res.status}`), { status: res.status });
  }
  const data = await res.json().catch(() => null);
  const scopes = typeof data?.scope === 'string' ? data.scope.split(' ') : [];
  if (typeof data?.access_token !== 'string' || data.access_token.length === 0 || data.access_token.length > 512) {
    throw new Error('token exchange returned no access token');
  }
  if (String(data.token_type).toLowerCase() !== 'bearer') throw new Error('token exchange returned an unexpected token type');
  if (!OAUTH_SCOPES.every((s) => scopes.includes(s))) throw new Error('token exchange is missing a required scope');
  const expiresIn = Number(data.expires_in);
  return {
    accessToken: data.access_token,
    expiresIn: Number.isFinite(expiresIn) && expiresIn > 0 ? expiresIn : SESSION_MAX_AGE_SECONDS,
  };
}

/**
 * Revokes an access token. Best effort: never throws.
 * @param {{ config: object, fetchImpl?: typeof fetch }} ctx
 * @param {string} accessToken
 * @returns {Promise<boolean>} true when Discord accepted the revocation
 */
export async function revokeToken({ config, fetchImpl = fetch }, accessToken) {
  try {
    const res = await postForm(fetchImpl, OAuth2Routes.tokenRevocationURL, config, {
      token: accessToken,
      token_type_hint: 'access_token',
    });
    await res.body?.cancel?.().catch(() => {});
    return res.ok;
  } catch {
    return false;
  }
}

// ── Access rules ──────────────────────────────────────────────────────────────

/**
 * Whether a user may manage a guild from the dashboard (the bot's ADMIN level): the guild must be in
 * the user's OAuth guild list, and the user owns it, has Administrator or Manage Server there, or is
 * BOT_OWNER_ID.
 * @param {{ owner?: boolean, permissions?: string }|null|undefined} oauthGuild entry of GET /users/@me/guilds
 * @param {string} userId
 * @param {string|null} [botOwnerId]
 * @returns {boolean}
 */
export function canManageGuild(oauthGuild, userId, botOwnerId = null) {
  if (!oauthGuild || typeof oauthGuild !== 'object') return false;
  if (botOwnerId && typeof userId === 'string' && userId === botOwnerId) return true;
  if (oauthGuild.owner === true) return true;
  const raw = oauthGuild.permissions;
  if (typeof raw !== 'string' || !/^\d{1,30}$/.test(raw)) return false;
  const perms = BigInt(raw);
  return (perms & ADMINISTRATOR) === ADMINISTRATOR || (perms & MANAGE_GUILD) === MANAGE_GUILD;
}

const header = (req, name) => {
  const value = req.headers?.[name];
  return Array.isArray(value) ? value[0] : value;
};

/** Whether an Origin header is a well-formed http(s) origin (not `null`, not a path). */
function isHttpOrigin(value) {
  try {
    const url = new URL(value);
    return (url.protocol === 'http:' || url.protocol === 'https:') && url.origin === value;
  } catch {
    return false;
  }
}

/**
 * CSRF guard for state-changing requests: POST/PATCH/DELETE only, a JSON content type, `X-Dashboard: 1`,
 * an Origin (when sent) equal to the dashboard's origin, and Sec-Fetch-Site (when sent) same-origin or none.
 * A different but well-formed http(s) Origin gets a message naming the configured origin (never the request's).
 * @param {import('node:http').IncomingMessage} req
 * @param {{ origin: string }} config
 * @returns {{ ok: true } | { ok: false, status: number, error: string, message: string }}
 */
export function checkWriteRequest(req, config) {
  if (req.method !== 'POST' && req.method !== 'PATCH' && req.method !== 'DELETE') {
    return { ok: false, status: 405, error: 'method_not_allowed', message: 'Method not allowed.' };
  }
  const contentType = String(header(req, 'content-type') ?? '').trim().toLowerCase();
  if (!contentType.startsWith('application/json')) {
    return { ok: false, status: 415, error: 'invalid_request', message: 'Content-Type must be application/json.' };
  }
  if (header(req, 'x-dashboard') !== '1') {
    return { ok: false, status: 403, error: 'forbidden', message: 'Missing the X-Dashboard header.' };
  }
  const origin = header(req, 'origin');
  if (origin !== undefined && origin !== config.origin) {
    // A well-formed origin that merely differs is most likely the wrong address in DASHBOARD_URL: say which one is expected.
    const message = isHttpOrigin(origin)
      ? `This dashboard is configured for ${config.origin}. Open it there, or set DASHBOARD_URL to the address you use.`
      : 'Cross-origin requests are not allowed.';
    return { ok: false, status: 403, error: 'forbidden', message };
  }
  const site = header(req, 'sec-fetch-site');
  if (site !== undefined && site !== 'same-origin' && site !== 'none') {
    return { ok: false, status: 403, error: 'forbidden', message: 'Cross-site requests are not allowed.' };
  }
  return { ok: true };
}
