// ── SSRF-safe HTTP client ──────────────────────────────────────────────────────
// Every request is checked twice: the hostname is resolved and validated up front, and the
// validated address list is then pinned into the socket's DNS lookup, which re-checks every
// address it hands to the connection. Redirects are followed by hand and repeat every check.
// A timeout, a response-size cap (counted after decompression) and manual redirect handling
// bound the damage a hostile URL can do. Every async export throws only SafeFetchError.
import dns from 'node:dns';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import zlib from 'node:zlib';
import { assertImageDimensions } from './imageSize.js';

export const DEFAULT_USER_AGENT = `DiscordBot/1.0 (+self-hosted; contact: ${process.env.HTTP_CONTACT || 'n/a'})`;

const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308]);
const DEFAULT_TIMEOUT_MS = 10_000;
const DEFAULT_MAX_BYTES = 2 * 1024 * 1024;
const DEFAULT_IMAGE_MAX_BYTES = 8 * 1024 * 1024;

// ── Errors ─────────────────────────────────────────────────────────────────────

/**
 * code: 'INVALID_URL' | 'BLOCKED_ADDRESS' | 'DNS_FAILED' | 'TIMEOUT' | 'TOO_LARGE' |
 *       'TOO_MANY_REDIRECTS' | 'HTTP_ERROR' | 'NETWORK' | 'BAD_CONTENT_TYPE' | 'BAD_IMAGE'
 * `new SafeFetchError('HTTP_ERROR', 404)` is accepted: the message becomes 'HTTP 404'.
 */
export class SafeFetchError extends Error {
  /**
   * @param {string} code
   * @param {string|number} [message] - a number is taken as the HTTP status
   * @param {{ status?: number, retryAfterMs?: number, headers?: Record<string, string>, cause?: unknown }} [extra]
   */
  constructor(code, message, { status, retryAfterMs, headers, cause } = {}) {
    if (typeof message === 'number') {
      status ??= message;
      message = `HTTP ${message}`;
    }
    super(message ?? code, cause ? { cause } : undefined);
    this.name = 'SafeFetchError';
    this.code = code;
    if (status !== undefined) this.status = status;
    if (retryAfterMs !== undefined) this.retryAfterMs = retryAfterMs;
    if (headers) this.headers = headers;
  }
}

// ── Address checks ─────────────────────────────────────────────────────────────

const BLOCKED_V4 = [
  ['0.0.0.0', 8], ['10.0.0.0', 8], ['100.64.0.0', 10], ['127.0.0.0', 8], ['169.254.0.0', 16],
  ['172.16.0.0', 12], ['192.0.0.0', 24], ['192.0.2.0', 24], ['192.88.99.0', 24], ['192.168.0.0', 16],
  ['198.18.0.0', 15], ['198.51.100.0', 24], ['203.0.113.0', 24], ['224.0.0.0', 4], ['240.0.0.0', 4],
];
const BLOCKED_V6 = [
  ['::', 128], ['::1', 128], ['fc00::', 7], ['fe80::', 10], ['fec0::', 10], ['ff00::', 8],
  ['2001:db8::', 32], ['2001::', 32], ['100::', 64],
  ['::', 96], // IPv4-compatible (deprecated): ::7f00:1 is ::127.0.0.1
];

let blockListCache = null;

function blockList() {
  if (!blockListCache) {
    const bl = new net.BlockList();
    for (const [addr, prefix] of BLOCKED_V4) bl.addSubnet(addr, prefix, 'ipv4');
    for (const [addr, prefix] of BLOCKED_V6) bl.addSubnet(addr, prefix, 'ipv6');
    blockListCache = bl;
  }
  return blockListCache;
}

// Expands an IPv6 literal to 8 numeric groups; handles '::' and a trailing dotted quad.
function expandIPv6(input) {
  let s = input;
  const lastColon = s.lastIndexOf(':');
  const tail = s.slice(lastColon + 1);
  if (tail.includes('.')) {
    const v4 = tail.split('.').map(Number);
    if (v4.length !== 4 || v4.some((n) => !Number.isInteger(n) || n < 0 || n > 255)) return null;
    s = `${s.slice(0, lastColon + 1)}${((v4[0] << 8) | v4[1]).toString(16)}:${((v4[2] << 8) | v4[3]).toString(16)}`;
  }
  const halves = s.split('::');
  if (halves.length > 2) return null;
  const head = halves[0] ? halves[0].split(':') : [];
  let groups;
  if (halves.length === 2) {
    const rest = halves[1] ? halves[1].split(':') : [];
    const fill = 8 - head.length - rest.length;
    if (fill < 0) return null;
    groups = [...head, ...Array(fill).fill('0'), ...rest];
  } else {
    groups = head;
  }
  if (groups.length !== 8) return null;
  const nums = groups.map((g) => (/^[0-9a-f]{1,4}$/.test(g) ? parseInt(g, 16) : NaN));
  return nums.some(Number.isNaN) ? null : nums;
}

const v4From = (hi, lo) => `${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`;

/**
 * True when an address must not be connected to: private, loopback, link-local, CGNAT,
 * cloud metadata, multicast, reserved, and IPv6 forms that embed such an IPv4 address.
 * Anything that is not an IP address is blocked (fail closed). Never throws.
 * @param {unknown} ip
 * @returns {boolean}
 */
export function isBlockedAddress(ip) {
  try {
    let s = String(ip ?? '').trim();
    if (s.startsWith('[') && s.endsWith(']')) s = s.slice(1, -1);
    const zone = s.indexOf('%');
    if (zone !== -1) s = s.slice(0, zone);
    s = s.toLowerCase();
    const fam = net.isIP(s);
    if (fam === 0) return true;
    const bl = blockList();
    if (fam === 4) return bl.check(s, 'ipv4');
    if (bl.check(s, 'ipv6')) return true;
    const g = expandIPv6(s);
    if (!g) return true;
    let embedded = null;
    if (g.slice(0, 5).every((x) => x === 0) && g[5] === 0xffff) embedded = v4From(g[6], g[7]); // v4-mapped
    else if (g.slice(0, 4).every((x) => x === 0) && g[4] === 0xffff && g[5] === 0) embedded = v4From(g[6], g[7]); // v4-translated (SIIT, RFC 2765)
    else if (g[0] === 0x64 && g[1] === 0xff9b && g.slice(2, 6).every((x) => x === 0)) embedded = v4From(g[6], g[7]); // NAT64
    else if (g[0] === 0x64 && g[1] === 0xff9b && g[2] === 1) embedded = v4From(g[6], g[7]); // local-use NAT64 (RFC 8215)
    else if (g[0] === 0x2002) embedded = v4From(g[1], g[2]); // 6to4
    return embedded !== null ? bl.check(embedded, 'ipv4') : false;
  } catch {
    return true;
  }
}

const stripBrackets = (host) => (host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host);

function withDeadline(promise, ms, timeoutError) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(timeoutError), Math.max(0, ms));
    timer.unref?.();
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

// Parses and validates a URL and resolves its host. Steps 1-5 (scheme, credentials, IP
// literals) run before any DNS call. Returns the URL and the validated, IPv4-first addresses.
async function resolvePublic(input, deadline = Date.now() + DEFAULT_TIMEOUT_MS, timeoutMs = DEFAULT_TIMEOUT_MS) {
  let u;
  try {
    u = input instanceof URL ? new URL(input.href) : new URL(String(input));
  } catch {
    throw new SafeFetchError('INVALID_URL', 'That is not a valid URL.');
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') {
    throw new SafeFetchError('INVALID_URL', 'Only http and https URLs are allowed.');
  }
  if (u.username || u.password) {
    throw new SafeFetchError('INVALID_URL', 'URLs with a username or password are not allowed.');
  }
  // URL has already normalised decimal, hex and octal IPv4 forms ('2130706433' is 127.0.0.1),
  // so always check u.hostname, never the raw string.
  const host = stripBrackets(u.hostname);
  if (!host || host.length > 253) throw new SafeFetchError('INVALID_URL', 'That is not a valid URL.');

  const family = net.isIP(host);
  if (family) {
    if (isBlockedAddress(host)) {
      throw new SafeFetchError('BLOCKED_ADDRESS', 'That address points to a private or reserved network.');
    }
    return { url: u, addresses: [{ address: host, family }] };
  }

  const remaining = deadline - Date.now();
  if (remaining <= 0) throw new SafeFetchError('TIMEOUT', `Request timed out after ${timeoutMs} ms.`);
  let found;
  try {
    found = await withDeadline(
      dns.promises.lookup(host, { all: true, verbatim: true }),
      remaining,
      new SafeFetchError('TIMEOUT', `Request timed out after ${timeoutMs} ms.`),
    );
  } catch (err) {
    if (err instanceof SafeFetchError) throw err;
    throw new SafeFetchError('DNS_FAILED', `Could not resolve ${host}.`, { cause: err });
  }
  if (!Array.isArray(found) || found.length === 0) {
    throw new SafeFetchError('DNS_FAILED', `Could not resolve ${host}.`);
  }
  if (found.some((a) => isBlockedAddress(a.address))) {
    throw new SafeFetchError('BLOCKED_ADDRESS', 'That address points to a private or reserved network.');
  }
  const addresses = [...found.filter((a) => a.family === 4), ...found.filter((a) => a.family !== 4)]
    .map(({ address, family: fam }) => ({ address, family: fam }));
  return { url: u, addresses };
}

/**
 * Parses a URL and resolves its host, refusing anything that is not a public http(s) address.
 * IP literals are checked before any DNS lookup.
 * @param {string|URL} url
 * @returns {Promise<URL>}
 * @throws {SafeFetchError} INVALID_URL, BLOCKED_ADDRESS, DNS_FAILED or TIMEOUT
 */
export async function assertPublicUrl(url) {
  return (await resolvePublic(url)).url;
}

// ── Request machinery ──────────────────────────────────────────────────────────

const lowerKeys = (obj) => Object.fromEntries(Object.entries(obj ?? {}).map(([k, v]) => [String(k).toLowerCase(), v]));

function parseRetryAfter(h) {
  if (h === undefined || h === null) return undefined;
  const s = String(h).trim();
  if (/^\d+$/.test(s)) return Number(s) * 1000;
  const date = Date.parse(s);
  return Number.isNaN(date) ? undefined : Math.max(0, date - Date.now());
}

function httpError(res) {
  return new SafeFetchError('HTTP_ERROR', `HTTP ${res.status}`, {
    status: res.status,
    headers: res.headers,
    retryAfterMs: res.status === 429 || res.status === 503 ? parseRetryAfter(res.headers['retry-after']) : undefined,
  });
}

// The socket's DNS lookup. It only ever returns addresses from the pinned list, and it
// re-checks each one, so a connection can never be made to a blocked address.
function pinnedLookup(target) {
  return (hostname, options, cb) => {
    if (typeof options === 'function') { cb = options; options = {}; }
    let list = target.addresses.filter((a) => !isBlockedAddress(a.address));
    const wanted = Number(options?.family);
    if (wanted === 4 || wanted === 6) list = list.filter((a) => a.family === wanted);
    if (list.length === 0) {
      cb(Object.assign(new Error('Blocked address'), { code: 'BLOCKED_ADDRESS' }));
      return;
    }
    if (options?.all) cb(null, list.map(({ address, family }) => ({ address, family })));
    else cb(null, list[0].address, list[0].family);
  };
}

function requestOnce({ target, method, headers, body, signal }) {
  return new Promise((resolve, reject) => {
    const u = target.url;
    const transport = u.protocol === 'https:' ? https : http;
    const req = transport.request({
      protocol: u.protocol,
      host: stripBrackets(u.hostname),
      port: u.port || undefined,
      path: u.pathname + u.search,
      method,
      headers,
      agent: false,
      signal,
      lookup: pinnedLookup(target),
    });
    req.once('response', resolve);
    req.once('error', reject);
    if (body && method !== 'GET' && method !== 'HEAD') req.end(body);
    else req.end();
  });
}

function decoderFor(encoding) {
  switch (encoding) {
    case 'gzip': case 'x-gzip': return zlib.createGunzip();
    case 'deflate': return zlib.createInflate();
    case 'br': return zlib.createBrotliDecompress();
    default: return null;
  }
}

// Reads the (decompressed) body, counting bytes after decompression. Destroys the response
// and the decoder on overflow, on abort and on any stream error.
function readBody(res, { maxBytes, signal, timeoutMs }) {
  return new Promise((resolve, reject) => {
    const decoder = decoderFor(String(res.headers['content-encoding'] ?? '').trim().toLowerCase());
    const chunks = [];
    let size = 0;
    let settled = false;
    const finish = (err, value) => {
      if (settled) return;
      settled = true;
      signal.removeEventListener('abort', onAbort);
      if (err) {
        res.destroy();
        decoder?.destroy();
        reject(err);
      } else {
        resolve(value);
      }
    };
    const onAbort = () => finish(new SafeFetchError('TIMEOUT', `Request timed out after ${timeoutMs} ms.`));
    if (signal.aborted) { onAbort(); return; }
    signal.addEventListener('abort', onAbort, { once: true });

    const out = decoder ? res.pipe(decoder) : res;
    out.on('data', (chunk) => {
      size += chunk.length;
      if (size > maxBytes) finish(new SafeFetchError('TOO_LARGE', `The response is larger than ${maxBytes} bytes.`));
      else chunks.push(chunk);
    });
    out.on('end', () => finish(null, Buffer.concat(chunks)));
    res.on('error', (err) => finish(new SafeFetchError('NETWORK', `Network error: ${err?.code ?? err?.message}`, { cause: err })));
    decoder?.on('error', (err) => finish(new SafeFetchError('NETWORK', `Network error: ${err?.code ?? err?.message}`, { cause: err })));
    res.on('close', () => {
      if (!res.complete) finish(new SafeFetchError('NETWORK', 'Network error: connection closed early'));
    });
  });
}

function decodeText(buffer, contentType) {
  const label = /charset\s*=\s*"?([^;"\s]+)/i.exec(contentType ?? '')?.[1] ?? 'utf-8';
  let decoder;
  try {
    decoder = new TextDecoder(label);
  } catch {
    decoder = new TextDecoder('utf-8');
  }
  return decoder.decode(buffer);
}

function flattenHeaders(raw) {
  const out = {};
  for (const [k, v] of Object.entries(raw ?? {})) out[k] = Array.isArray(v) ? v.join(', ') : String(v);
  return out;
}

/**
 * Fetches a public http(s) URL with SSRF protection, a timeout and a size cap.
 * Redirects are followed manually (up to `maxRedirects`), and each hop repeats the URL and
 * address checks. A 304 comes back as `{ ok: false, status: 304 }` for conditional GETs.
 * @param {string|URL} url
 * @param {{ method?: string, headers?: Record<string, string>, body?: object|string|Buffer|Uint8Array|null,
 *   timeoutMs?: number, maxBytes?: number, maxRedirects?: number }} [opts]
 * @returns {Promise<{ ok: boolean, status: number, statusText: string, headers: Record<string, string>,
 *   url: string, buffer: Buffer, text(): string, json(): any }>}
 * @throws {SafeFetchError} never anything else
 */
export async function safeFetch(url, {
  method = 'GET', headers = {}, body = null,
  timeoutMs = DEFAULT_TIMEOUT_MS, maxBytes = DEFAULT_MAX_BYTES, maxRedirects = 5,
} = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  const deadline = Date.now() + timeoutMs;
  try {
    let verb = String(method ?? 'GET').toUpperCase();
    const hdrs = {
      'user-agent': DEFAULT_USER_AGENT,
      accept: '*/*',
      'accept-encoding': 'gzip, deflate, br',
      ...lowerKeys(headers),
    };
    delete hdrs.host;

    let payload = null;
    if (body !== null && body !== undefined && verb !== 'GET' && verb !== 'HEAD') {
      if (typeof body === 'string' || Buffer.isBuffer(body) || body instanceof Uint8Array) {
        payload = body;
      } else if (typeof body === 'object') {
        payload = JSON.stringify(body);
        hdrs['content-type'] ??= 'application/json';
      }
      if (payload !== null) hdrs['content-length'] = String(Buffer.byteLength(payload));
    }

    let hop = 0;
    let target = await resolvePublic(url, deadline, timeoutMs);
    for (;;) {
      const res = await requestOnce({ target, method: verb, headers: hdrs, body: payload, signal: controller.signal });
      const status = res.statusCode ?? 0;
      const location = res.headers.location;

      if (REDIRECT_STATUSES.has(status) && location) {
        res.resume();
        if (++hop > maxRedirects) throw new SafeFetchError('TOO_MANY_REDIRECTS', 'Too many redirects.');
        let next;
        try {
          next = new URL(String(location), target.url);
        } catch {
          throw new SafeFetchError('INVALID_URL', 'The server sent an invalid redirect.');
        }
        if (status === 303 || ((status === 301 || status === 302) && verb === 'POST')) {
          verb = 'GET';
          payload = null;
          delete hdrs['content-type'];
          delete hdrs['content-length'];
        }
        if (next.origin !== target.url.origin) {
          delete hdrs.authorization;
          delete hdrs.cookie;
        }
        target = await resolvePublic(next, deadline, timeoutMs); // repeats every URL and address check
        continue;
      }

      const declared = Number(res.headers['content-length']);
      if (Number.isFinite(declared) && declared > maxBytes) {
        res.destroy();
        throw new SafeFetchError('TOO_LARGE', `The response is larger than ${maxBytes} bytes.`);
      }
      let buffer = Buffer.alloc(0);
      if (verb === 'HEAD') res.resume();
      else buffer = await readBody(res, { maxBytes, signal: controller.signal, timeoutMs });

      const flat = flattenHeaders(res.headers);
      return {
        ok: status >= 200 && status < 300,
        status,
        statusText: res.statusMessage ?? '',
        headers: flat,
        url: target.url.href,
        buffer,
        text() { return decodeText(buffer, flat['content-type']); },
        json() { return JSON.parse(decodeText(buffer, flat['content-type'])); },
      };
    }
  } catch (err) {
    throw mapError(err, controller, timeoutMs);
  } finally {
    clearTimeout(timer);
  }
}

function mapError(err, controller, timeoutMs) {
  if (err instanceof SafeFetchError) return err;
  if (err?.code === 'BLOCKED_ADDRESS') {
    return new SafeFetchError('BLOCKED_ADDRESS', 'That address points to a private or reserved network.', { cause: err });
  }
  if (err?.name === 'AbortError' || controller.signal.aborted) {
    return new SafeFetchError('TIMEOUT', `Request timed out after ${timeoutMs} ms.`, { cause: err });
  }
  return new SafeFetchError('NETWORK', `Network error: ${err?.code ?? err?.message ?? 'unknown'}`, { cause: err });
}

// ── Convenience wrappers ───────────────────────────────────────────────────────

function withHeaders(opts, defaults) {
  return { ...opts, headers: { ...defaults, ...lowerKeys(opts?.headers) } };
}

/**
 * GET/POST and parse JSON.
 * @param {string|URL} url
 * @param {object} [opts] - safeFetch options
 * @returns {Promise<any>}
 * @throws {SafeFetchError} HTTP_ERROR (with .status, .headers, .retryAfterMs) when not ok or not JSON
 */
export async function safeFetchJson(url, opts = {}) {
  const res = await safeFetch(url, withHeaders(opts, { accept: 'application/json' }));
  if (!res.ok) throw httpError(res);
  try {
    return res.json();
  } catch {
    throw new SafeFetchError('HTTP_ERROR', 'Response is not valid JSON', { status: res.status });
  }
}

/**
 * @param {string|URL} url
 * @param {object} [opts] - safeFetch options
 * @returns {Promise<string>}
 * @throws {SafeFetchError} HTTP_ERROR when the response is not ok
 */
export async function safeFetchText(url, opts = {}) {
  const res = await safeFetch(url, opts);
  if (!res.ok) throw httpError(res);
  return res.text();
}

/**
 * Downloads an image. The content type must be image/* (not SVG) and the header must declare
 * a sane size, so no caller can hand a decompression bomb to the canvas decoder.
 * @param {string|URL} url
 * @param {{ maxBytes?: number, timeoutMs?: number, maxSide?: number, maxPixels?: number }} [opts]
 * @returns {Promise<Buffer>}
 * @throws {SafeFetchError} HTTP_ERROR, BAD_CONTENT_TYPE or BAD_IMAGE
 */
export async function safeFetchImage(url, {
  maxBytes = DEFAULT_IMAGE_MAX_BYTES, timeoutMs = 8_000, maxSide, maxPixels, ...rest
} = {}) {
  const res = await safeFetch(url, { ...withHeaders(rest, { accept: 'image/*' }), maxBytes, timeoutMs });
  if (!res.ok) throw httpError(res);
  const type = String(res.headers['content-type'] ?? '').split(';')[0].trim().toLowerCase();
  if (!/^image\//.test(type) || type === 'image/svg+xml') {
    throw new SafeFetchError('BAD_CONTENT_TYPE', 'That URL is not an image.');
  }
  try {
    assertImageDimensions(res.buffer, { ...(maxSide ? { maxSide } : {}), ...(maxPixels ? { maxPixels } : {}) });
  } catch (err) {
    throw new SafeFetchError('BAD_IMAGE', 'That image is too large or is not a supported format.', { cause: err });
  }
  return res.buffer;
}
