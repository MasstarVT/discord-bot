// Web dashboard configuration: every environment variable the dashboard reads is parsed and validated here.
// Pure: nothing is read at import time; dashboard.js (repo root) calls loadDashboardConfig() once at start.

import { isIP } from 'node:net';

export const DASHBOARD_DEFAULTS = Object.freeze({
  url: 'http://localhost:8080',
  port: 8080,
  host: '0.0.0.0',
});

/** Keys the dashboard cannot run without (the bot itself needs only the last four). */
export const REQUIRED_KEYS = Object.freeze([
  'CLIENT_ID', 'DISCORD_TOKEN', 'DISCORD_CLIENT_SECRET', 'DASHBOARD_SESSION_SECRET', 'DATABASE_URL', 'REDIS_URL',
]);

export const MIN_SESSION_SECRET_LENGTH = 32;
const SNOWFLAKE_RE = /^\d{17,20}$/;
const EMAIL_RE = /^[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+$/;

/** An IP address in the form `req.socket.remoteAddress` reports it: lower case, IPv4-mapped IPv6 unwrapped. */
export function normalizeIp(value) {
  const ip = String(value ?? '').trim().toLowerCase();
  return ip.startsWith('::ffff:') && isIP(ip.slice(7)) === 4 ? ip.slice(7) : ip;
}

/**
 * Parses DASHBOARD_CONTACT, the contact shown on the privacy and terms pages: an email address or an https URL.
 * @returns {{ kind: 'email'|'url', value: string }|null} null when it is neither
 */
export function parseContact(raw) {
  if (raw.length > 254) return null;
  if (EMAIL_RE.test(raw)) return { kind: 'email', value: raw };
  try {
    const url = new URL(raw);
    if (url.protocol === 'https:' && !url.username && !url.password) return { kind: 'url', value: url.href };
  } catch { /* not a URL */ }
  return null;
}

const read = (env, key) => {
  const value = env?.[key];
  return typeof value === 'string' ? value.trim() : '';
};

/**
 * Parses DASHBOARD_URL into its public origin. Only a bare origin is accepted (a path, query, fragment
 * or credentials would break the redirects and cookie paths, which all assume the dashboard owns `/`).
 * @param {string} raw
 * @returns {{ ok: true, url: string } | { ok: false, reason: string }}
 */
export function parseDashboardUrl(raw) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    return { ok: false, reason: 'must be an absolute http(s) URL such as https://dash.example.com' };
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return { ok: false, reason: 'must use http or https' };
  if (url.username || url.password) return { ok: false, reason: 'must not contain credentials' };
  if (url.pathname !== '/' || url.search || url.hash) {
    return { ok: false, reason: 'must be the bare origin (no path, query or fragment), e.g. https://dash.example.com' };
  }
  return { ok: true, url: url.origin };
}

/**
 * Reads and validates the dashboard configuration. Never throws.
 * `config` is always filled (best effort, defaults for anything invalid) so a parked server can still
 * listen on the configured port; it is only safe to serve the dashboard when `ok` is true.
 * @param {Record<string, string|undefined>} [env]
 * @returns {{
 *   ok: boolean,
 *   missing: string[],
 *   invalid: { key: string, reason: string }[],
 *   warnings: string[],
 *   config: {
 *     clientId: string, botToken: string, clientSecret: string, sessionSecret: string,
 *     databaseUrl: string, redisUrl: string, botOwnerId: string|null,
 *     url: string, urlIsDefault: boolean, origin: string, redirectUri: string, secureCookies: boolean,
 *     port: number, host: string, trustedProxies: readonly string[],
 *     contact: { kind: 'email'|'url', value: string }|null, supportUrl: string|null,
 *   },
 * }}
 */
export function loadDashboardConfig(env = process.env) {
  const missing = REQUIRED_KEYS.filter((key) => read(env, key) === '');
  const invalid = [];
  const warnings = [];

  const clientId = read(env, 'CLIENT_ID');
  if (clientId && !SNOWFLAKE_RE.test(clientId)) invalid.push({ key: 'CLIENT_ID', reason: 'must be the application id (17-20 digits)' });

  const sessionSecret = read(env, 'DASHBOARD_SESSION_SECRET');
  if (sessionSecret && sessionSecret.length < MIN_SESSION_SECRET_LENGTH) {
    invalid.push({ key: 'DASHBOARD_SESSION_SECRET', reason: `must be at least ${MIN_SESSION_SECRET_LENGTH} characters` });
  }

  let url = DASHBOARD_DEFAULTS.url;
  const rawUrl = read(env, 'DASHBOARD_URL');
  if (rawUrl) {
    const parsed = parseDashboardUrl(rawUrl.replace(/\/+$/, ''));
    if (parsed.ok) url = parsed.url;
    else invalid.push({ key: 'DASHBOARD_URL', reason: parsed.reason });
  }

  let port = DASHBOARD_DEFAULTS.port;
  const rawPort = read(env, 'DASHBOARD_PORT');
  if (rawPort) {
    const n = Number(rawPort);
    if (/^\d{1,5}$/.test(rawPort) && n >= 1 && n <= 65_535) port = n;
    else invalid.push({ key: 'DASHBOARD_PORT', reason: 'must be a port number from 1 to 65535' });
  }

  const host = read(env, 'DASHBOARD_HOST') || DASHBOARD_DEFAULTS.host;

  let botOwnerId = read(env, 'BOT_OWNER_ID') || null;
  if (botOwnerId && !SNOWFLAKE_RE.test(botOwnerId)) {
    warnings.push('BOT_OWNER_ID is not a Discord user id (17-20 digits); it is ignored by the dashboard.');
    botOwnerId = null;
  }

  // Reverse proxies whose X-Forwarded-For is believed (the login rate limit's client address); loopback always is.
  const trustedProxies = [];
  const rawProxies = read(env, 'DASHBOARD_TRUSTED_PROXIES');
  if (rawProxies) {
    const parts = rawProxies.split(',').map((s) => s.trim()).filter(Boolean);
    if (parts.every((p) => isIP(p))) trustedProxies.push(...new Set(parts.map(normalizeIp)));
    else invalid.push({ key: 'DASHBOARD_TRUSTED_PROXIES', reason: 'must be a comma-separated list of IP addresses' });
  }

  let contact = null;
  const rawContact = read(env, 'DASHBOARD_CONTACT');
  if (rawContact) {
    contact = parseContact(rawContact);
    if (!contact) warnings.push('DASHBOARD_CONTACT is not an email address or an https URL; the privacy and terms pages show no contact.');
  }

  // The support server invite on the public pages; the pages show no support server link without it.
  let supportUrl = null;
  const rawSupport = read(env, 'DASHBOARD_SUPPORT_URL');
  if (rawSupport) {
    const parsed = parseContact(rawSupport);
    if (parsed?.kind === 'url') supportUrl = parsed.value;
    else warnings.push('DASHBOARD_SUPPORT_URL is not an https URL; the public pages show no support server link.');
  }

  const config = Object.freeze({
    clientId,
    botToken: read(env, 'DISCORD_TOKEN'),
    clientSecret: read(env, 'DISCORD_CLIENT_SECRET'),
    sessionSecret,
    databaseUrl: read(env, 'DATABASE_URL'),
    redisUrl: read(env, 'REDIS_URL'),
    botOwnerId,
    url,
    origin: new URL(url).origin,
    urlIsDefault: rawUrl === '',
    redirectUri: `${url}/auth/callback`,
    secureCookies: url.startsWith('https:'),
    port,
    host,
    trustedProxies: Object.freeze(trustedProxies),
    contact: contact && Object.freeze(contact),
    supportUrl,
  });

  return { ok: missing.length === 0 && invalid.length === 0, missing, invalid, warnings, config };
}
