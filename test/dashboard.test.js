// Web dashboard tests: the catalog and its schema agreement, configuration parsing, session sealing, the
// access and CSRF rules, the Discord client's caching, and the HTTP API against fake Discord and settings
// dependencies. Offline: no Discord, Postgres or Redis (src/dashboard/settingsBridge.js is never imported).
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { request } from 'node:http';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
process.env.DATABASE_URL  ??= 'postgresql://test:test@127.0.0.1:1/test';
process.env.REDIS_URL     ??= 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const { MODULES, MODULE_NAMES } = await import('../src/config/modules.js');
const catalog = await import('../src/dashboard/catalog.js');
const auth = await import('../src/dashboard/auth.js');
const { loadDashboardConfig, REQUIRED_KEYS } = await import('../src/dashboard/config.js');
const { createDashboardServer, createParkedServer, createRateLimiter, BODY_LIMIT_BYTES } = await import('../src/dashboard/server.js');
const discordApi = await import('../src/dashboard/discordApi.js');
const { PermissionFlagsBits } = await import('discord.js');

const { TOGGLE_FIELDS, TOGGLE_DEFAULTS, SUB_TOGGLES, CATEGORIES, DESCRIPTIONS, buildCatalog, pickToggleValues, validateChanges } = catalog;
const { DiscordApiError } = discordApi;

// ── Shared fixtures ──────────────────────────────────────────────────────────

const SECRET = 'x'.repeat(48);
const KEY = auth.deriveSessionKey(SECRET);
const USER_ID = '111111111111111111';
const OWNER_ID = '999999999999999999';
const TOKEN = 'user-access-token-abc123';
const G = Object.freeze({
  zeta: '200000000000000001', // Manage Server, bot present
  alpha: '200000000000000002', // owner, bot absent
  beta: '200000000000000003', // Administrator, bot present
  plain: '200000000000000004', // no permission, bot present
  broken: '200000000000000005', // settings cannot be read
  stranger: '200000000000000099', // not in the user's guild list
});

function baseEnv(extra = {}) {
  return {
    CLIENT_ID: '123456789012345678',
    DISCORD_TOKEN: 'bot-token',
    DISCORD_CLIENT_SECRET: 'client-secret',
    DASHBOARD_SESSION_SECRET: SECRET,
    DATABASE_URL: 'postgresql://u:p@localhost/db',
    REDIS_URL: 'redis://localhost:6379',
    ...extra,
  };
}

function schemaBooleans() {
  const schema = readFileSync(join(ROOT, 'prisma/schema.prisma'), 'utf8');
  const model = /^model GuildSettings \{([\s\S]*?)^\}/m.exec(schema)?.[1] ?? '';
  const out = new Map();
  for (const m of model.matchAll(/^\s*(\w+)\s+Boolean(\?)?(?:\s+@default\((true|false)\))?/gm)) {
    out.set(m[1], { optional: m[2] === '?', default: m[3] === undefined ? undefined : m[3] === 'true' });
  }
  return out;
}

// ── Catalog ──────────────────────────────────────────────────────────────────

describe('catalog', () => {
  test('every MODULES key appears in exactly one category, in the contract order', () => {
    assert.deepEqual(CATEGORIES.map((c) => c.id), ['safety', 'community', 'engagement', 'tools']);
    const listed = CATEGORIES.flatMap((c) => c.modules);
    assert.equal(listed.length, new Set(listed).size, 'a module is listed twice');
    assert.deepEqual([...listed].sort(), [...MODULE_NAMES].sort());
    for (const c of CATEGORIES) {
      assert.ok(c.label && c.description, `${c.id} needs a label and a description`);
    }
  });

  test('every module has a short description (a sentence or two)', () => {
    for (const key of MODULE_NAMES) {
      const text = DESCRIPTIONS[key];
      assert.equal(typeof text, 'string', `${key} has no description`);
      assert.ok(text.length >= 20 && text.length <= 140, `${key}: description length ${text.length}`);
      assert.match(text, /\.$/, `${key}: description should be a sentence`);
    }
    assert.deepEqual(Object.keys(DESCRIPTIONS).sort(), [...MODULE_NAMES].sort());
  });

  test('the Live Role description warns that switching it off keeps the role on current streamers', () => {
    assert.match(DESCRIPTIONS.liverole, /off here leaves it on current streamers/);
    assert.match(DESCRIPTIONS.liverole, /\/liverole disable in Discord removes it too/);
  });

  test('buildCatalog() mirrors the module registry', () => {
    const c = buildCatalog();
    assert.deepEqual(Object.keys(c.modules).sort(), [...MODULE_NAMES].sort());
    for (const key of MODULE_NAMES) {
      const m = c.modules[key];
      assert.equal(m.key, key);
      assert.equal(m.flag, MODULES[key].flag);
      assert.equal(m.label, MODULES[key].label);
      assert.equal(m.emoji, MODULES[key].emoji);
      assert.equal(m.configCommand, MODULES[key].configCommand);
      assert.equal(m.default, MODULES[key].default);
      assert.equal(m.description, DESCRIPTIONS[key]);
      assert.ok(Array.isArray(m.subToggles));
      for (const t of m.subToggles) assert.deepEqual(Object.keys(t).sort(), ['default', 'description', 'field', 'label']);
    }
    assert.deepEqual(c.modules.moderation.subToggles.map((t) => t.field), ['modDmTargets']);
    assert.ok(Object.isFrozen(c) && Object.isFrozen(c.modules.welcome.subToggles));
    // Round-trips through JSON unchanged (it is served as is).
    assert.deepEqual(JSON.parse(JSON.stringify(c)), c);
  });

  test('every sub-toggle is a Boolean GuildSettings column whose @default matches the catalog', () => {
    const columns = schemaBooleans();
    const subs = Object.values(SUB_TOGGLES).flat();
    assert.equal(subs.length, 16);
    for (const t of subs) {
      const col = columns.get(t.field);
      assert.ok(col, `${t.field} is not a Boolean column of GuildSettings`);
      assert.equal(col.optional, false, `${t.field} must not be nullable`);
      assert.equal(col.default, t.default, `${t.field}: schema default ${col.default}, catalog ${t.default}`);
      assert.ok(t.label && t.description, `${t.field} needs a label and a description`);
    }
    for (const key of Object.keys(SUB_TOGGLES)) assert.ok(MODULE_NAMES.includes(key), `${key} is not a module`);
  });

  test('every module flag is a Boolean column whose @default matches MODULES', () => {
    const columns = schemaBooleans();
    for (const key of MODULE_NAMES) {
      const col = columns.get(MODULES[key].flag);
      assert.ok(col, `${MODULES[key].flag} is not a Boolean column`);
      assert.equal(col.default, MODULES[key].default, `${MODULES[key].flag}`);
    }
  });

  test('TOGGLE_FIELDS holds the module flags and sub-toggles only, and is read-only', () => {
    assert.equal(TOGGLE_FIELDS.size, MODULE_NAMES.length + 16);
    for (const key of MODULE_NAMES) assert.ok(TOGGLE_FIELDS.has(MODULES[key].flag));
    for (const field of ['levelingEnabled', 'captchaLockdownPrior', 'xpRate', 'guildId', 'timezone', 'customPrefix']) {
      assert.equal(TOGGLE_FIELDS.has(field), false, `${field} must not be a toggle`);
    }
    assert.throws(() => TOGGLE_FIELDS.add('levelingEnabled'), TypeError);
    assert.throws(() => TOGGLE_FIELDS.delete('modEnabled'), TypeError);
    assert.equal(TOGGLE_FIELDS.has('levelingEnabled'), false);
    assert.equal(Object.keys(TOGGLE_DEFAULTS).length, TOGGLE_FIELDS.size);
  });

  test('pickToggleValues returns every toggle, falling back to the default', () => {
    const fromNull = pickToggleValues(null);
    assert.deepEqual(Object.keys(fromNull).sort(), [...TOGGLE_FIELDS].sort());
    assert.deepEqual(fromNull, { ...TOGGLE_DEFAULTS });
    const values = pickToggleValues({ modEnabled: false, xpEnabled: 'yes', voiceXpEnabled: true, levelingEnabled: true });
    assert.equal(values.modEnabled, false);
    assert.equal(values.xpEnabled, false); // non-boolean: the default
    assert.equal(values.voiceXpEnabled, true);
    assert.equal('levelingEnabled' in values, false);
  });

  test('validateChanges accepts strict booleans for known fields', () => {
    assert.deepEqual(validateChanges({ changes: { modEnabled: false, voiceXpEnabled: true } }),
      { ok: true, changes: { modEnabled: false, voiceXpEnabled: true } });
    const all = Object.fromEntries([...TOGGLE_FIELDS].map((f) => [f, true]));
    assert.equal(validateChanges({ changes: all }).ok, true);
  });

  test('validateChanges rejects bad bodies', () => {
    const bad = [
      null, [], 'x', 1,
      {},
      { changes: null },
      { changes: [] },
      { changes: {} },
      { changes: { modEnabled: 'true' } },
      { changes: { modEnabled: 1 } },
      { changes: { modEnabled: null } },
      { changes: { notAField: true } },
      { changes: { levelingEnabled: true } },
      { changes: { captchaLockdownPrior: false } },
      { changes: { modEnabled: true }, extra: 1 },
      { changes: Object.fromEntries(Array.from({ length: 61 }, (_, i) => [`f${i}`, true])) },
      JSON.parse('{"changes":{"__proto__":true}}'),
      JSON.parse('{"changes":{"constructor":true}}'),
      JSON.parse('{"__proto__":{"polluted":true},"changes":{"modEnabled":true}}'),
    ];
    for (const body of bad) {
      const r = validateChanges(body);
      assert.equal(r.ok, false, `accepted ${JSON.stringify(body)}`);
      assert.equal(typeof r.message, 'string');
    }
    assert.equal(({}).polluted, undefined);
    assert.match(validateChanges({ changes: Object.fromEntries(Array.from({ length: 61 }, (_, i) => [`f${i}`, true])) }).message, /60/);
  });
});

// ── Configuration ────────────────────────────────────────────────────────────

describe('config', () => {
  test('reports every missing required key', () => {
    const r = loadDashboardConfig({});
    assert.equal(r.ok, false);
    assert.deepEqual(r.missing, [...REQUIRED_KEYS]);
    assert.equal(r.config.port, 8080);
    assert.equal(r.config.host, '0.0.0.0');
  });

  test('defaults and derived values', () => {
    const r = loadDashboardConfig(baseEnv());
    assert.equal(r.ok, true, JSON.stringify(r.invalid));
    assert.equal(r.config.url, 'http://localhost:8080');
    assert.equal(r.config.origin, 'http://localhost:8080');
    assert.equal(r.config.redirectUri, 'http://localhost:8080/auth/callback');
    assert.equal(r.config.secureCookies, false);
    assert.equal(r.config.botOwnerId, null);
    assert.equal(r.config.urlIsDefault, true);
    assert.equal(loadDashboardConfig({}).config.urlIsDefault, true);
  });

  test('urlIsDefault is false once DASHBOARD_URL is set, even to the default address', () => {
    for (const url of ['https://dash.example.com', 'http://localhost:8080']) {
      const r = loadDashboardConfig(baseEnv({ DASHBOARD_URL: url }));
      assert.equal(r.config.urlIsDefault, false, url);
      assert.equal(r.config.url, url);
    }
    assert.equal(loadDashboardConfig(baseEnv({ DASHBOARD_URL: '   ' })).config.urlIsDefault, true, 'blank counts as unset');
  });

  test('an https DASHBOARD_URL makes cookies secure; the trailing slash is dropped', () => {
    const r = loadDashboardConfig(baseEnv({ DASHBOARD_URL: 'https://dash.example.com/', DASHBOARD_PORT: '9000', BOT_OWNER_ID: OWNER_ID }));
    assert.equal(r.ok, true);
    assert.equal(r.config.url, 'https://dash.example.com');
    assert.equal(r.config.redirectUri, 'https://dash.example.com/auth/callback');
    assert.equal(r.config.secureCookies, true);
    assert.equal(r.config.port, 9000);
    assert.equal(r.config.botOwnerId, OWNER_ID);
  });

  test('invalid values are reported by key', () => {
    const cases = [
      [{ DASHBOARD_SESSION_SECRET: 'short' }, 'DASHBOARD_SESSION_SECRET'],
      [{ DASHBOARD_URL: 'ftp://example.com' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'not a url' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'https://example.com/dashboard' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_URL: 'https://user:pw@example.com' }, 'DASHBOARD_URL'],
      [{ DASHBOARD_PORT: '70000' }, 'DASHBOARD_PORT'],
      [{ DASHBOARD_PORT: 'abc' }, 'DASHBOARD_PORT'],
      [{ CLIENT_ID: 'my-app' }, 'CLIENT_ID'],
    ];
    for (const [extra, key] of cases) {
      const r = loadDashboardConfig(baseEnv(extra));
      assert.equal(r.ok, false, `${JSON.stringify(extra)} was accepted`);
      assert.deepEqual(r.invalid.map((i) => i.key), [key]);
    }
    const owner = loadDashboardConfig(baseEnv({ BOT_OWNER_ID: 'me' }));
    assert.equal(owner.ok, true);
    assert.equal(owner.config.botOwnerId, null);
    assert.equal(owner.warnings.length, 1);
  });

  test('every key the dashboard reads is documented in .env.example', () => {
    const env = readFileSync(join(ROOT, '.env.example'), 'utf8');
    for (const key of ['DISCORD_CLIENT_SECRET', 'DASHBOARD_URL', 'DASHBOARD_SESSION_SECRET', 'DASHBOARD_PORT', 'DASHBOARD_HOST']) {
      assert.match(env, new RegExp(`^#\\s*${key}=`, 'm'), `${key} must be a commented-out line`);
    }
  });
});

// ── Sessions, cookies and access rules ───────────────────────────────────────

describe('auth', () => {
  const payload = { uid: USER_ID, username: 'alice', globalName: 'Alice', avatar: null, accessToken: TOKEN, exp: Date.now() + 60_000 };

  test('seal/unseal round-trips and hides the payload', () => {
    const sealed = auth.sealSession(payload, KEY);
    assert.match(sealed, /^v1\.[A-Za-z0-9_-]+$/);
    assert.equal(sealed.includes(TOKEN), false);
    assert.equal(Buffer.from(sealed.slice(3), 'base64url').toString('latin1').includes(TOKEN), false);
    assert.deepEqual(auth.unsealSession(sealed, KEY), payload);
    assert.notEqual(auth.sealSession(payload, KEY), sealed, 'a fresh IV per seal');
  });

  test('tampered, expired, foreign or malformed values unseal to null', () => {
    const sealed = auth.sealSession(payload, KEY);
    const body = sealed.slice(3);
    const mid = Math.floor(body.length / 2);
    const flipped = `v1.${body.slice(0, mid)}${body[mid] === 'A' ? 'B' : 'A'}${body.slice(mid + 1)}`;
    assert.equal(auth.unsealSession(flipped, KEY), null);
    assert.equal(auth.unsealSession(sealed.slice(0, -4), KEY), null);
    assert.equal(auth.unsealSession(`v2.${body}`, KEY), null);
    assert.equal(auth.unsealSession(sealed, auth.deriveSessionKey('y'.repeat(48))), null);
    assert.equal(auth.unsealSession(sealed, KEY, payload.exp), null, 'expired at exp');
    assert.equal(auth.unsealSession(auth.sealSession({ ...payload, exp: 'never' }, KEY), KEY), null);
    assert.equal(auth.unsealSession(auth.sealSession([1, 2], KEY), KEY), null);
    for (const junk of [undefined, null, '', 'v1.', 'v1.!!!', 'abc', 'v1.' + 'A'.repeat(5000), 42]) {
      assert.equal(auth.unsealSession(junk, KEY), null);
    }
  });

  test('cookies parse and serialise', () => {
    const jar = auth.parseCookies('a=1; dash_session=v1.abc; a=2; bad=%E0%A4%A; q="quoted"; =x; flag');
    assert.equal(jar.get('a'), '1');
    assert.equal(jar.get('dash_session'), 'v1.abc');
    assert.equal(jar.has('bad'), false);
    assert.equal(jar.get('q'), 'quoted');
    assert.equal(auth.parseCookies(undefined).size, 0);
    const c = auth.serializeCookie('dash_session', 'v1.x', { maxAge: 60.9, secure: true });
    assert.equal(c, 'dash_session=v1.x; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure');
    assert.match(auth.clearCookie('dash_session'), /^dash_session=; Path=\/; HttpOnly; SameSite=Lax; Max-Age=0; Expires=/);
  });

  test('the OAuth state is random and compared exactly', () => {
    const s = auth.createOAuthState();
    assert.match(s, /^[A-Za-z0-9_-]{43}$/);
    assert.notEqual(auth.createOAuthState(), s);
    assert.equal(auth.statesMatch(s, s), true);
    assert.equal(auth.statesMatch(s, `${s}x`), false);
    assert.equal(auth.statesMatch(s, s.toUpperCase()), false);
    assert.equal(auth.statesMatch(undefined, s), false);
    assert.equal(auth.statesMatch('', ''), false);
    assert.equal(auth.statesMatch(s, null), false);
  });

  test('the authorize URL asks for identify and guilds', () => {
    const url = new URL(auth.buildAuthorizeUrl({ clientId: '123456789012345678', redirectUri: 'https://d.example/auth/callback' }, 'st'));
    assert.equal(url.origin, 'https://discord.com');
    assert.equal(url.pathname, '/oauth2/authorize');
    assert.equal(url.searchParams.get('scope'), 'identify guilds');
    assert.equal(url.searchParams.get('response_type'), 'code');
    assert.equal(url.searchParams.get('redirect_uri'), 'https://d.example/auth/callback');
    assert.equal(url.searchParams.get('state'), 'st');
    assert.equal(url.searchParams.get('prompt'), 'none');
  });

  test('canManageGuild mirrors the ADMIN level', () => {
    const cases = [
      [{ owner: true, permissions: '0' }, USER_ID, null, true],
      [{ owner: false, permissions: '8' }, USER_ID, null, true],
      [{ owner: false, permissions: '32' }, USER_ID, null, true],
      [{ owner: false, permissions: '1099511627775' }, USER_ID, null, true],
      [{ owner: false, permissions: '0' }, USER_ID, null, false],
      [{ owner: false, permissions: '16' }, USER_ID, null, false],
      [{ owner: false, permissions: '1099511627735' }, USER_ID, null, false], // every bit but 0x8 and 0x20
      [{ owner: false, permissions: 'abc' }, USER_ID, null, false],
      [{ owner: false, permissions: '-8' }, USER_ID, null, false],
      [{ owner: false }, USER_ID, null, false],
      [{ owner: 'true', permissions: '0' }, USER_ID, null, false],
      [{ owner: false, permissions: '0' }, OWNER_ID, OWNER_ID, true],
      [{ owner: false, permissions: '0' }, USER_ID, OWNER_ID, false],
      [undefined, OWNER_ID, OWNER_ID, false], // the guild must be in the user's list
      [null, USER_ID, null, false],
    ];
    for (const [guild, uid, owner, expected] of cases) {
      assert.equal(auth.canManageGuild(guild, uid, owner), expected, JSON.stringify([guild, uid, owner]));
    }
  });

  test('checkWriteRequest blocks cross-site writes', () => {
    const config = { origin: 'https://dash.example.com' };
    const req = (method, headers) => ({ method, headers });
    const json = { 'content-type': 'application/json', 'x-dashboard': '1' };
    const cases = [
      [req('GET', json), 405],
      [req('DELETE', json), 405],
      [req('POST', { 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'text/plain', 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'application/x-www-form-urlencoded', 'x-dashboard': '1' }), 415],
      [req('POST', { 'content-type': 'application/json' }), 403],
      [req('POST', { ...json, 'x-dashboard': 'true' }), 403],
      [req('PATCH', { ...json, origin: 'https://evil.example' }), 403],
      [req('PATCH', { ...json, origin: 'null' }), 403],
      [req('PATCH', { ...json, origin: 'https://dash.example.com.evil.example' }), 403],
      [req('PATCH', { ...json, 'sec-fetch-site': 'cross-site' }), 403],
      [req('PATCH', { ...json, 'sec-fetch-site': 'same-site' }), 403],
      [req('PATCH', json), 200],
      [req('POST', { ...json, 'content-type': 'Application/JSON; charset=utf-8' }), 200],
      [req('PATCH', { ...json, origin: 'https://dash.example.com', 'sec-fetch-site': 'same-origin' }), 200],
      [req('POST', { ...json, 'sec-fetch-site': 'none' }), 200],
    ];
    for (const [r, status] of cases) {
      const result = auth.checkWriteRequest(r, config);
      if (status === 200) assert.deepEqual(result, { ok: true }, JSON.stringify(r));
      else assert.equal(result.status, status, JSON.stringify(r));
    }
  });

  test('checkWriteRequest names the configured address when a well-formed Origin differs', () => {
    const config = { origin: 'https://dash.example.com' };
    const json = { 'content-type': 'application/json', 'x-dashboard': '1' };
    const check = (origin) => auth.checkWriteRequest({ method: 'PATCH', headers: { ...json, origin } }, config);
    const expected = 'This dashboard is configured for https://dash.example.com. Open it there, or set DASHBOARD_URL to the address you use.';
    for (const origin of ['http://192.168.1.5:8080', 'http://localhost:8080', 'https://evil.example', 'https://dash.example.com.evil.example']) {
      const result = check(origin);
      assert.deepEqual([result.ok, result.status, result.error], [false, 403, 'forbidden'], origin);
      assert.equal(result.message, expected, origin);
      assert.equal(result.message.includes(new URL(origin).host), false, 'the request origin is never echoed');
    }
    // Not a well-formed http(s) origin: the generic message.
    for (const origin of ['null', '', 'not an origin', 'ftp://dash.example.com', 'https://evil.example/path', 'javascript:alert(1)']) {
      const result = check(origin);
      assert.equal(result.status, 403, JSON.stringify(origin));
      assert.equal(result.message, 'Cross-origin requests are not allowed.', JSON.stringify(origin));
    }
    // The matching origin still passes, and Sec-Fetch-Site keeps its own message.
    assert.deepEqual(check('https://dash.example.com'), { ok: true });
    const site = auth.checkWriteRequest({ method: 'PATCH', headers: { ...json, origin: config.origin, 'sec-fetch-site': 'cross-site' } }, config);
    assert.equal(site.message, 'Cross-site requests are not allowed.');
  });
});

// ── Discord client ───────────────────────────────────────────────────────────

describe('discordApi', () => {
  const config = { clientId: '123456789012345678', clientSecret: 's', botToken: 'bot-token', redirectUri: 'http://localhost:8080/auth/callback' };

  /** A fake fetch: `routes(url, init)` returns [status, body] or throws. Records every call. */
  function fakeFetch(routes) {
    const calls = [];
    const impl = async (url, init) => {
      calls.push({ url: String(url), auth: init?.headers?.Authorization });
      const [status, body, headers = {}] = await routes(String(url), init);
      return new Response(body === undefined ? null : JSON.stringify(body), { status, headers });
    };
    return { impl, calls };
  }

  test('CDN URLs and the invite link', () => {
    const api = discordApi.createDiscordApi({ config, fetchImpl: async () => { throw new Error('no network'); } });
    assert.equal(discordApi.userAvatarUrl({ id: '80351110224678912', avatar: null }),
      `https://cdn.discordapp.com/embed/avatars/${Number((80351110224678912n >> 22n) % 6n)}.png`);
    const hash = 'a'.repeat(32);
    assert.equal(discordApi.userAvatarUrl({ id: USER_ID, avatar: hash }), `https://cdn.discordapp.com/avatars/${USER_ID}/${hash}.webp?size=128`);
    assert.match(discordApi.userAvatarUrl({ id: USER_ID, avatar: '../../x' }), /\/embed\/avatars\/\d\.png$/);
    assert.equal(discordApi.guildIconUrl({ id: G.zeta, icon: null }), null);
    assert.equal(discordApi.guildIconUrl({ id: G.zeta, icon: hash }), `https://cdn.discordapp.com/icons/${G.zeta}/${hash}.webp?size=128`);

    const invite = new URL(api.inviteUrl(G.zeta));
    assert.equal(invite.origin, 'https://discord.com');
    assert.equal(invite.searchParams.get('scope'), 'bot applications.commands');
    assert.equal(invite.searchParams.get('guild_id'), G.zeta);
    assert.equal(invite.searchParams.get('disable_guild_select'), 'true');
    const perms = BigInt(invite.searchParams.get('permissions'));
    assert.equal(perms & PermissionFlagsBits.Administrator, 0n, 'never Administrator');
    for (const p of ['ManageRoles', 'ManageChannels', 'BanMembers', 'KickMembers', 'ModerateMembers', 'ManageMessages', 'ManageGuild', 'SendMessages']) {
      assert.equal(perms & PermissionFlagsBits[p], PermissionFlagsBits[p], p);
    }
  });

  test('getUserGuilds caches per user for 60 s and shares concurrent calls', async () => {
    let t = 1_000_000;
    const { impl, calls } = fakeFetch(async () => [200, [{ id: G.zeta, name: 'Z', icon: null, owner: false, permissions: '32' }, { id: 'bad' }]]);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    const [a, b] = await Promise.all([api.getUserGuilds(USER_ID, TOKEN), api.getUserGuilds(USER_ID, TOKEN)]);
    assert.equal(calls.length, 1);
    assert.equal(a, b);
    assert.deepEqual(a.map((g) => g.id), [G.zeta], 'invalid entries are dropped');
    assert.equal(calls[0].auth, `Bearer ${TOKEN}`);
    t += 59_000;
    await api.getUserGuilds(USER_ID, TOKEN);
    assert.equal(calls.length, 1);
    t += 2_000;
    await api.getUserGuilds(USER_ID, TOKEN);
    assert.equal(calls.length, 2);
    await api.getUserGuilds(USER_ID, 'another-token');
    assert.equal(calls.length, 3, 'a different token does not reuse the cache');
    api.forgetUser(USER_ID);
    await api.getUserGuilds(USER_ID, 'another-token');
    assert.equal(calls.length, 4);
  });

  test('getUserGuilds maps 401 and 429, serving a recent list on 429', async () => {
    let t = 0;
    let reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    const { impl } = fakeFetch(async () => reply);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    await api.getUserGuilds(USER_ID, TOKEN);
    t += 61_000;
    reply = [429, { retry_after: 2.5 }];
    assert.deepEqual((await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true })).map((g) => g.id), [G.zeta], 'stale list on 429');
    await assert.rejects(api.getUserGuilds(OWNER_ID, TOKEN), (err) => err instanceof DiscordApiError && err.code === 'rate_limited' && err.retryAfter === 3);
    reply = [401, { message: '401: Unauthorized' }];
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unauthenticated');
    const down = discordApi.createDiscordApi({ config, fetchImpl: async () => { throw new TypeError('fetch failed'); } });
    await assert.rejects(down.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unavailable');
  });

  test('getUserGuilds serves an old list only when the caller allows it, and never past 5 minutes', async () => {
    let t = 0;
    let reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    const { impl, calls } = fakeFetch(async () => reply);
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    await api.getUserGuilds(USER_ID, TOKEN);
    t += 61_000; // past the 60 s TTL, within the 5 minute stale window
    const failures = [[429, { retry_after: 2.5 }, 'rate_limited'], [503, {}, 'unavailable']];
    for (const [status, body, code] of failures) {
      reply = [status, body];
      for (const opts of [undefined, {}, { allowStale: false }]) {
        await assert.rejects(api.getUserGuilds(USER_ID, TOKEN, opts), (err) => err instanceof DiscordApiError && err.code === code,
          `${status} ${JSON.stringify(opts)}: authoritative callers get the error`);
      }
      assert.deepEqual((await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true })).map((g) => g.id), [G.zeta], `${status}: stale list for display`);
    }
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN), (err) => err.code === 'unavailable' && err.status === 503);

    // Callers sharing one in-flight request each decide for themselves.
    reply = [429, { retry_after: 2.5 }];
    const before = calls.length;
    const [strict, lenient] = await Promise.allSettled([
      api.getUserGuilds(USER_ID, TOKEN), api.getUserGuilds(USER_ID, TOKEN, { allowStale: true }),
    ]);
    assert.equal(calls.length, before + 1, 'one request');
    assert.equal(strict.status, 'rejected');
    assert.equal(strict.reason.code, 'rate_limited');
    assert.equal(strict.reason.retryAfter, 3);
    assert.equal(lenient.status, 'fulfilled');

    t += 5 * 60_000; // the list is now older than 5 minutes
    await assert.rejects(api.getUserGuilds(USER_ID, TOKEN, { allowStale: true }), (err) => err.code === 'rate_limited');

    // A fresh list is served from the cache either way, without a request.
    reply = [200, [{ id: G.zeta, name: 'Z', owner: true, permissions: '0' }]];
    await api.getUserGuilds(USER_ID, TOKEN);
    const cached = calls.length;
    await api.getUserGuilds(USER_ID, TOKEN);
    await api.getUserGuilds(USER_ID, TOKEN, { allowStale: true });
    assert.equal(calls.length, cached);
  });

  test('isBotInGuild pages through the bot guild list and double-checks misses', async () => {
    const page1 = Array.from({ length: 200 }, (_, i) => ({ id: String(300000000000000000n + BigInt(i)) }));
    const page2 = [{ id: G.zeta }];
    const { impl, calls } = fakeFetch(async (url) => {
      if (url.includes('/users/@me/guilds')) return [200, new URL(url).searchParams.get('after') === '0' ? page1 : page2];
      if (url.endsWith(`/guilds/${G.beta}`)) return [200, { id: G.beta }];
      if (url.endsWith(`/guilds/${G.alpha}`)) return [403, { code: 50001 }];
      return [404, { code: 10004 }];
    });
    let t = 0;
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, now: () => t });
    assert.equal(await api.isBotInGuild(G.zeta), true);
    assert.equal(calls.length, 2, 'two pages');
    assert.equal(calls[1].url.includes(`after=${page1[199].id}`), true);
    assert.equal(calls[0].auth, 'Bot bot-token');
    assert.equal(await api.isBotInGuild(page1[5].id), true);
    assert.equal(calls.length, 2, 'cached');
    assert.equal(await api.isBotInGuild(G.alpha, { verify: false }), false);
    assert.equal(calls.length, 2, 'verify:false never double-checks');
    assert.equal(await api.isBotInGuild(G.alpha), false);
    assert.equal(await api.isBotInGuild(G.stranger), false);
    assert.equal(await api.isBotInGuild(G.beta), true);
    const before = calls.length;
    assert.equal(await api.isBotInGuild(G.beta), true, 'a confirmed guild is remembered');
    assert.equal(calls.length, before);
    assert.equal(await api.isBotInGuild('not-a-snowflake'), false);
  });

  /** A fake fetch for the bot's guild list: page 1 is full, page 2 holds G.zeta; `limit(url)` may answer a 429 first. */
  function pagedBotGuilds(limit) {
    const page1 = Array.from({ length: 200 }, (_, i) => ({ id: String(300000000000000000n + BigInt(i)) }));
    return fakeFetch(async (url) => {
      const after = new URL(url).searchParams.get('after');
      const limited = limit(after);
      if (limited) return limited;
      return [200, after === '0' ? page1 : [{ id: G.zeta }]];
    });
  }

  test('the bot guild load waits out a short 429 mid-pagination and retries the same page', async () => {
    let limits = 1;
    const { impl, calls } = pagedBotGuilds((after) => (after !== '0' && limits-- > 0 ? [429, { retry_after: 0.01 }] : null));
    const sleeps = [];
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl, sleep: async (ms) => { sleeps.push(ms); } });
    const ids = await api.getBotGuildIds();
    assert.equal(ids.size, 201);
    assert.equal(ids.has(G.zeta), true);
    assert.equal(calls.length, 3, 'page 1, the limited page 2, page 2 again');
    assert.equal(calls[2].url, calls[1].url, 'the same page is retried');
    assert.deepEqual(sleeps, [1000], 'retry_after is rounded up to whole seconds');
    assert.equal(await api.isBotInGuild(G.zeta, { verify: false }), true);
    assert.equal(calls.length, 3, 'cached afterwards');
  });

  test('the bot guild load fails on a long 429 and after three retries', async () => {
    // A wait longer than 10 s is not worth holding the request for.
    const long = pagedBotGuilds((after) => (after !== '0' ? [429, { retry_after: 30 }] : null));
    const longSleeps = [];
    const api = discordApi.createDiscordApi({ config, fetchImpl: long.impl, sleep: async (ms) => { longSleeps.push(ms); } });
    await assert.rejects(api.getBotGuildIds(), (err) => err instanceof DiscordApiError && err.code === 'rate_limited' && err.retryAfter === 30);
    assert.equal(long.calls.length, 2);
    assert.deepEqual(longSleeps, []);

    // Exactly 10 s is still waited out; the retries are capped at 3 per refresh, counted across pages.
    const stuck = pagedBotGuilds((after) => (after !== '0' ? [429, { retry_after: 10 }] : null));
    const stuckSleeps = [];
    const stuckApi = discordApi.createDiscordApi({ config, fetchImpl: stuck.impl, sleep: async (ms) => { stuckSleeps.push(ms); } });
    await assert.rejects(stuckApi.getBotGuildIds(), (err) => err.code === 'rate_limited');
    assert.deepEqual(stuckSleeps, [10_000, 10_000, 10_000]);
    assert.equal(stuck.calls.length, 1 + 4, 'page 1, then the limited page 2 once and three retries');

    // Other failures are not retried.
    const down = pagedBotGuilds((after) => (after !== '0' ? [503, {}] : null));
    const downSleeps = [];
    const downApi = discordApi.createDiscordApi({ config, fetchImpl: down.impl, sleep: async (ms) => { downSleeps.push(ms); } });
    await assert.rejects(downApi.getBotGuildIds(), (err) => err.code === 'unavailable');
    assert.deepEqual(downSleeps, []);
    assert.equal(down.calls.length, 2);
  });

  test('the default pause between bot guild page retries is a real, unref\'d wait', async () => {
    let limits = 1;
    const { impl, calls } = pagedBotGuilds((after) => (after !== '0' && limits-- > 0 ? [429, { retry_after: 1 }] : null));
    const api = discordApi.createDiscordApi({ config, fetchImpl: impl });
    const started = Date.now();
    const keepAlive = setTimeout(() => {}, 5_000); // the pause itself is unref'd; in the server the listener keeps the loop alive
    try {
      assert.equal((await api.getBotGuildIds()).size, 201);
    } finally {
      clearTimeout(keepAlive);
    }
    assert.ok(Date.now() - started >= 900, 'waited about retry_after seconds');
    assert.equal(calls.length, 3);
  });
});

// ── HTTP API ─────────────────────────────────────────────────────────────────

const USER_GUILDS = Object.freeze([
  { id: G.zeta, name: 'zeta', icon: null, owner: false, permissions: '32' },
  { id: G.alpha, name: 'Alpha', icon: 'b'.repeat(32), owner: true, permissions: '0' },
  { id: G.beta, name: 'beta', icon: null, owner: false, permissions: '8' },
  { id: G.plain, name: 'Plain', icon: null, owner: false, permissions: '1024' },
  { id: G.broken, name: 'Broken', icon: null, owner: true, permissions: '0' },
]);
const BOT_GUILDS = new Set([G.zeta, G.beta, G.plain, G.broken]);

function createFakes() {
  const calls = { writeToggles: [], exchangeCode: [], revokeToken: [], forgetUser: [], getUserGuilds: [] };
  let row = {
    guildId: G.zeta, modEnabled: true, xpEnabled: false, voiceXpEnabled: true, levelingEnabled: true,
    captchaLockdownPrior: true, timezone: 'Europe/Berlin', customPrefix: '', embedColor: 0x5865f2,
  };
  const discord = {
    async getUserGuilds(uid, token, opts = {}) {
      calls.getUserGuilds.push({ token, allowStale: opts?.allowStale === true });
      // Stands in for a Discord 429 or outage with an older list on hand: only a caller that allows it gets the list.
      if (token === 'stale-limited-token' || token === 'stale-down-token') {
        if (opts?.allowStale === true) return USER_GUILDS;
        throw token === 'stale-limited-token'
          ? new DiscordApiError('Discord returned HTTP 429', { status: 429, code: 'rate_limited', retryAfter: 4 })
          : new DiscordApiError('Discord request failed', { code: 'unavailable' });
      }
      if (token === 'expired-token') throw new DiscordApiError('Discord returned HTTP 401', { status: 401, code: 'unauthenticated' });
      if (token === 'limited-token') throw new DiscordApiError('Discord returned HTTP 429', { status: 429, code: 'rate_limited', retryAfter: 7 });
      if (token === 'down-token') throw new DiscordApiError('Discord request failed', { code: 'unavailable' });
      if (token === 'broken-token') return null;
      return USER_GUILDS;
    },
    async isBotInGuild(guildId) { return BOT_GUILDS.has(guildId); },
    inviteUrl: (guildId) => `https://discord.com/oauth2/authorize?client_id=123456789012345678&guild_id=${guildId}`,
    async exchangeCode(code) {
      calls.exchangeCode.push(code);
      if (code !== 'good-code') throw new Error('token exchange returned HTTP 400');
      return { accessToken: TOKEN, expiresIn: 604_800 };
    },
    async getCurrentUser() { return { id: USER_ID, username: 'alice', globalName: 'Alice', avatar: null }; },
    async revokeToken(token) { calls.revokeToken.push(token); return true; },
    forgetUser(uid) { calls.forgetUser.push(uid); },
  };
  const settings = {
    async readGuildSettings(guildId) {
      if (guildId === G.broken) throw new Error('database down');
      return { ...row, guildId };
    },
    async writeToggles(guildId, changes) {
      calls.writeToggles.push({ guildId, changes });
      if (guildId === G.broken) throw new Error('database down');
      row = { ...row, ...changes };
      return { ...row, guildId };
    },
  };
  return { discord, settings, calls };
}

function recordingLogger() {
  const lines = [];
  const rec = (level) => (...args) => lines.push(`${level} ${args.map((a) => (a instanceof Error ? a.message : String(a))).join(' ')}`);
  return { lines, info: rec('info'), warn: rec('warn'), error: rec('error'), debug: rec('debug'), success: rec('success') };
}

/** One HTTP request without keep-alive. Resolves { status, headers, text, json }. */
function call(port, method, path, { headers = {}, body } = {}) {
  return new Promise((resolvePromise, reject) => {
    const req = request({ host: '127.0.0.1', port, method, path, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8');
        let json;
        try { json = JSON.parse(text); } catch { json = undefined; }
        resolvePromise({ status: res.statusCode, headers: res.headers, text, json });
      });
    });
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

const listen = (server) => new Promise((res) => server.listen(0, '127.0.0.1', () => res(server.address().port)));
const close = (server) => new Promise((res) => { server.close(() => res()); server.closeAllConnections(); });

function sessionCookie(overrides = {}) {
  const sealed = auth.sealSession({
    uid: USER_ID, username: 'alice', globalName: 'Alice', avatar: null, accessToken: TOKEN, exp: Date.now() + 3_600_000, ...overrides,
  }, KEY);
  return `${auth.SESSION_COOKIE}=${sealed}`;
}

const WRITE_HEADERS = Object.freeze({ 'content-type': 'application/json', 'x-dashboard': '1', origin: 'http://localhost:8080' });

describe('HTTP server', () => {
  const { config } = loadDashboardConfig(baseEnv());
  const fakes = createFakes();
  const log = recordingLogger();
  let publicDir;
  let server;
  let port;
  const get = (path, headers = {}) => call(port, 'GET', path, { headers });
  const authed = (path, extra = {}) => get(path, { cookie: sessionCookie(), ...extra });
  const patch = (guildId, body, headers = {}) => call(port, 'PATCH', `/api/guilds/${guildId}/settings`, {
    headers: { cookie: sessionCookie(), ...WRITE_HEADERS, ...headers },
    body: typeof body === 'string' ? body : JSON.stringify(body),
  });

  before(async () => {
    publicDir = mkdtempSync(join(tmpdir(), 'dashboard-public-'));
    writeFileSync(join(publicDir, 'index.html'), '<!doctype html><title>t</title>');
    writeFileSync(join(publicDir, 'app.js'), 'export {};');
    server = createDashboardServer({ config, deps: { discord: fakes.discord, settings: fakes.settings }, logger: log, publicDir });
    port = await listen(server);
  });

  after(async () => {
    await close(server);
    rmSync(publicDir, { recursive: true, force: true });
  });

  test('GET /healthz answers with the security headers', async () => {
    const r = await get('/healthz');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, { ok: true });
    assert.match(r.headers['content-security-policy'], /default-src 'self'/);
    assert.match(r.headers['content-security-policy'], /frame-ancestors 'none'/);
    assert.match(r.headers['content-security-policy'], /img-src 'self' https:\/\/cdn\.discordapp\.com data:/);
    assert.equal(r.headers['x-content-type-options'], 'nosniff');
    assert.equal(r.headers['x-frame-options'], 'DENY');
    assert.equal(r.headers['referrer-policy'], 'same-origin');
    assert.equal(r.headers['cross-origin-opener-policy'], 'same-origin');
    assert.match(r.headers['permissions-policy'], /camera=\(\)/);
    assert.equal(r.headers['strict-transport-security'], undefined, 'no HSTS over http');
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.equal((await call(port, 'POST', '/healthz')).status, 405);
    const head = await call(port, 'HEAD', '/healthz');
    assert.equal(head.status, 200);
    assert.equal(head.text, '');
  });

  test('GET /api/me needs a valid session', async () => {
    const none = await get('/api/me');
    assert.equal(none.status, 401);
    assert.equal(none.json.error, 'unauthenticated');
    assert.equal(none.headers['set-cookie'], undefined);

    const forged = await get('/api/me', { cookie: `${auth.SESSION_COOKIE}=v1.forged` });
    assert.equal(forged.status, 401);
    assert.match(forged.headers['set-cookie'][0], /^dash_session=; .*Max-Age=0/);

    const expired = await get('/api/me', { cookie: sessionCookie({ exp: Date.now() - 1 }) });
    assert.equal(expired.status, 401);

    const badShape = await get('/api/me', { cookie: sessionCookie({ uid: 'x' }) });
    assert.equal(badShape.status, 401);

    const ok = await authed('/api/me');
    assert.equal(ok.status, 200);
    assert.deepEqual(ok.json, {
      user: { id: USER_ID, username: 'alice', globalName: 'Alice', avatarUrl: discordApi.userAvatarUrl({ id: USER_ID, avatar: null }) },
    });
    assert.equal(ok.text.includes(TOKEN), false, 'the access token never leaves the server');
  });

  test('GET /api/catalog', async () => {
    assert.equal((await get('/api/catalog')).status, 401);
    const r = await authed('/api/catalog');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json, JSON.parse(JSON.stringify(buildCatalog())));
  });

  test('GET /api/guilds lists manageable guilds, bot-present first, then by name', async () => {
    const r = await authed('/api/guilds');
    assert.equal(r.status, 200);
    assert.equal(r.headers['cache-control'], 'no-store');
    assert.deepEqual(r.json.guilds.map((g) => g.name), ['beta', 'Broken', 'zeta', 'Alpha']);
    const alpha = r.json.guilds.find((g) => g.id === G.alpha);
    assert.equal(alpha.botPresent, false);
    assert.equal(alpha.inviteUrl, fakes.discord.inviteUrl(G.alpha));
    assert.equal(alpha.iconUrl, discordApi.guildIconUrl({ id: G.alpha, icon: 'b'.repeat(32) }));
    const zeta = r.json.guilds.find((g) => g.id === G.zeta);
    assert.deepEqual(zeta, { id: G.zeta, name: 'zeta', iconUrl: null, botPresent: true, inviteUrl: null });
    assert.equal(r.json.guilds.some((g) => g.id === G.plain), false, 'no permission: not listed');
  });

  test('Discord failures on /api/guilds', async () => {
    const expired = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'expired-token' }) });
    assert.equal(expired.status, 401);
    assert.match(expired.headers['set-cookie'][0], /^dash_session=; /);
    const limited = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'limited-token' }) });
    assert.equal(limited.status, 429);
    assert.equal(limited.json.error, 'rate_limited');
    assert.equal(limited.json.retryAfter, 7);
    assert.equal(limited.headers['retry-after'], '7');
    const down = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'down-token' }) });
    assert.equal(down.status, 502);
    assert.equal(down.json.error, 'discord_unavailable');
  });

  test('only the read-only server list may use an old guild list; settings GET and PATCH never do', async () => {
    const uid = '444444444444444444';
    const writes = fakes.calls.writeToggles.length;
    for (const [accessToken, status, error] of [['stale-limited-token', 429, 'rate_limited'], ['stale-down-token', 502, 'discord_unavailable']]) {
      const cookie = sessionCookie({ uid, accessToken });
      const list = await authed('/api/guilds', { cookie });
      assert.equal(list.status, 200, `${accessToken}: the listing keeps the stale fallback`);
      assert.ok(list.json.guilds.some((g) => g.id === G.zeta));

      const read = await authed(`/api/guilds/${G.zeta}/settings`, { cookie });
      assert.equal(read.status, status, `${accessToken}: GET settings`);
      assert.equal(read.json.error, error);
      const write = await patch(G.zeta, { changes: { modEnabled: false } }, { cookie });
      assert.equal(write.status, status, `${accessToken}: PATCH settings`);
      assert.equal(write.json.error, error);
      if (status === 429) {
        for (const r of [read, write]) {
          assert.equal(r.json.retryAfter, 4);
          assert.equal(r.headers['retry-after'], '4');
        }
      }
    }
    assert.equal(fakes.calls.writeToggles.length, writes, 'nothing was written');
    const asked = (token) => fakes.calls.getUserGuilds.filter((c) => c.token === token).map((c) => c.allowStale);
    assert.deepEqual(asked('stale-limited-token'), [true, false, false]);
    assert.deepEqual(asked('stale-down-token'), [true, false, false]);
  });

  test('GET settings: 403 without permission, 404 without the bot, 200 shape', async () => {
    const plain = await authed(`/api/guilds/${G.plain}/settings`);
    assert.equal(plain.status, 403);
    assert.equal(plain.json.error, 'forbidden');
    assert.equal((await authed(`/api/guilds/${G.stranger}/settings`)).status, 403);
    assert.equal((await get(`/api/guilds/${G.zeta}/settings`)).status, 401);
    assert.equal((await authed('/api/guilds/abc/settings')).status, 400);

    const absent = await authed(`/api/guilds/${G.alpha}/settings`);
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');
    assert.equal(absent.json.inviteUrl, fakes.discord.inviteUrl(G.alpha));

    const r = await authed(`/api/guilds/${G.zeta}/settings`);
    assert.equal(r.status, 200);
    assert.deepEqual(Object.keys(r.json).sort(), ['general', 'guild', 'propagationSeconds', 'values']);
    assert.deepEqual(r.json.guild, { id: G.zeta, name: 'zeta', iconUrl: null });
    assert.deepEqual(Object.keys(r.json.values).sort(), [...TOGGLE_FIELDS].sort());
    assert.equal(r.json.values.modEnabled, true);
    assert.equal(r.json.values.voiceXpEnabled, true);
    assert.equal(r.json.values.welcomeUseEmbed, true, 'missing column reads as the default');
    assert.equal('levelingEnabled' in r.json.values, false);
    assert.deepEqual(r.json.general, { timezone: 'Europe/Berlin', customPrefix: '', embedColor: '#5865F2' });
    assert.equal(r.json.propagationSeconds, 30);

    const broken = await authed(`/api/guilds/${G.broken}/settings`);
    assert.equal(broken.status, 503);
    assert.equal(broken.json.error, 'settings_unavailable');
    assert.equal((await call(port, 'DELETE', `/api/guilds/${G.zeta}/settings`, { headers: { cookie: sessionCookie() } })).status, 405);
  });

  test('PATCH settings: CSRF checks come first', async () => {
    const before = fakes.calls.writeToggles.length;
    const body = { changes: { modEnabled: false } };
    const noHeader = await patch(G.zeta, body, { 'x-dashboard': '' });
    assert.equal(noHeader.status, 403);
    const wrongOrigin = await patch(G.zeta, body, { origin: 'https://evil.example' });
    assert.equal(wrongOrigin.status, 403);
    assert.equal(wrongOrigin.json.error, 'forbidden');
    assert.equal(wrongOrigin.json.message, 'This dashboard is configured for http://localhost:8080. Open it there, or set DASHBOARD_URL to the address you use.');
    assert.equal(wrongOrigin.text.includes('evil.example'), false, 'the request origin is not echoed');
    const nullOrigin = await patch(G.zeta, body, { origin: 'null' });
    assert.equal(nullOrigin.status, 403);
    assert.equal(nullOrigin.json.message, 'Cross-origin requests are not allowed.');
    assert.equal((await patch(G.zeta, body, { 'content-type': 'text/plain' })).status, 415);
    assert.equal((await patch(G.zeta, body, { 'sec-fetch-site': 'cross-site' })).status, 403);
    assert.equal((await call(port, 'PATCH', `/api/guilds/${G.zeta}/settings`, { headers: { ...WRITE_HEADERS }, body: JSON.stringify(body) })).status, 401);
    assert.equal(fakes.calls.writeToggles.length, before);
  });

  test('PATCH settings: 400 on a bad body, 413 on a large one', async () => {
    const before = fakes.calls.writeToggles.length;
    for (const body of ['{', '', 'null', '[]', { changes: {} }, { changes: { modEnabled: 'false' } }, { changes: { levelingEnabled: true } }, { changes: { nope: true } }]) {
      const r = await patch(G.zeta, body);
      assert.equal(r.status, 400, `body ${JSON.stringify(body)}`);
      assert.equal(r.json.error, 'invalid_request');
    }
    const big = await patch(G.zeta, JSON.stringify({ changes: { modEnabled: true }, pad: 'x'.repeat(BODY_LIMIT_BYTES) }));
    assert.equal(big.status, 413);
    assert.equal(fakes.calls.writeToggles.length, before);
  });

  test('PATCH settings: 403/404 before any write', async () => {
    const before = fakes.calls.writeToggles.length;
    assert.equal((await patch(G.plain, { changes: { modEnabled: false } })).status, 403);
    const absent = await patch(G.alpha, { changes: { modEnabled: false } });
    assert.equal(absent.status, 404);
    assert.equal(absent.json.error, 'bot_not_in_guild');
    assert.equal(fakes.calls.writeToggles.length, before);
  });

  test('PATCH settings: writes exactly the validated changes and returns every value', async () => {
    const r = await patch(G.zeta, { changes: { voiceXpEnabled: false, modEnabled: false } });
    assert.equal(r.status, 200);
    assert.deepEqual(fakes.calls.writeToggles.at(-1), { guildId: G.zeta, changes: { voiceXpEnabled: false, modEnabled: false } });
    assert.deepEqual(Object.keys(r.json), ['values']);
    assert.deepEqual(Object.keys(r.json.values).sort(), [...TOGGLE_FIELDS].sort());
    assert.equal(r.json.values.voiceXpEnabled, false);
    assert.equal(r.json.values.modEnabled, false);
    assert.ok(log.lines.some((l) => l.startsWith('info') && l.includes(G.zeta) && l.includes('voiceXpEnabled=false') && l.includes(USER_ID)));

    const failed = await patch(G.broken, { changes: { modEnabled: true } });
    assert.equal(failed.status, 500);
    assert.equal(failed.json.error, 'save_failed');
  });

  test('writes are rate limited per user', async () => {
    const cookie = sessionCookie({ uid: '333333333333333333' });
    let last;
    for (let i = 0; i < 21; i++) last = await patch(G.zeta, '{', { cookie });
    assert.equal(last.status, 429);
    assert.equal(last.json.error, 'rate_limited');
    assert.ok(last.json.retryAfter >= 1 && last.json.retryAfter <= 10);
    assert.equal((await patch(G.zeta, '{')).status, 400, 'other users are not affected');
  });

  test('a handler that throws answers 500 instead of crashing', async () => {
    const r = await authed('/api/guilds', { cookie: sessionCookie({ accessToken: 'broken-token' }) });
    assert.equal(r.status, 500);
    assert.equal(r.json.error, 'internal_error');
    assert.equal((await get('/healthz')).status, 200, 'still serving');
  });

  test('static files: whitelist only, no traversal', async () => {
    const index = await get('/');
    assert.equal(index.status, 200);
    assert.match(index.headers['content-type'], /^text\/html/);
    assert.match(index.headers['content-security-policy'], /script-src 'self'/);
    assert.equal((await get('/?error=login_failed')).status, 200);
    assert.equal((await get('/index.html')).text, index.text);
    const app = await get('/app.js');
    assert.equal(app.status, 200);
    assert.match(app.headers['content-type'], /^text\/javascript/);
    assert.equal((await get('/favicon.svg')).status, 404, 'missing file: 404, no crash');
    for (const path of ['/../package.json', '/%2e%2e/package.json', '/..%2fpackage.json', '/package.json', '/src/dashboard/server.js',
      '/public/app.js', '//etc/passwd', '/index.html/', '/app.js%00']) {
      const r = await get(path);
      assert.equal(r.status, 404, path);
    }
    assert.equal((await get('/api/nope')).json.error, 'not_found');
  });

  test('GET /auth/login redirects to Discord with a state cookie', async () => {
    const r = await get('/auth/login');
    assert.equal(r.status, 302);
    const location = new URL(r.headers.location);
    assert.equal(location.origin, 'https://discord.com');
    assert.equal(location.searchParams.get('client_id'), config.clientId);
    assert.equal(location.searchParams.get('scope'), 'identify guilds');
    assert.equal(location.searchParams.get('redirect_uri'), 'http://localhost:8080/auth/callback');
    const cookie = r.headers['set-cookie'][0];
    assert.match(cookie, /^dash_oauth_state=[A-Za-z0-9_-]{43}; Path=\/auth; HttpOnly; SameSite=Lax; Max-Age=600$/);
    assert.equal(cookie.split(';')[0].split('=')[1], location.searchParams.get('state'));
  });

  test('GET /auth/callback rejects a missing or mismatched state', async () => {
    const before = fakes.calls.exchangeCode.length;
    const cases = [
      ['/auth/callback?code=good-code&state=abc', `${auth.STATE_COOKIE}=different`],
      ['/auth/callback?code=good-code&state=abc', undefined],
      ['/auth/callback?code=good-code', `${auth.STATE_COOKIE}=abc`],
      ['/auth/callback?error=access_denied&state=abc', `${auth.STATE_COOKIE}=abc`],
      ['/auth/callback?state=abc', `${auth.STATE_COOKIE}=abc`],
    ];
    for (const [path, cookie] of cases) {
      const r = await get(path, cookie ? { cookie } : {});
      assert.equal(r.status, 302, path);
      assert.equal(r.headers.location, '/?error=login_failed');
      assert.ok(r.headers['set-cookie'].some((c) => /^dash_oauth_state=; Path=\/auth; .*Max-Age=0/.test(c)), 'state cookie cleared');
      assert.equal(r.headers['set-cookie'].some((c) => c.startsWith('dash_session=v1.')), false);
    }
    assert.equal(fakes.calls.exchangeCode.length, before, 'no code exchange without a matching state');
    const bad = await get('/auth/callback?code=bad-code&state=abc', { cookie: `${auth.STATE_COOKIE}=abc` });
    assert.equal(bad.headers.location, '/?error=login_failed');
  });

  test('GET /auth/callback logs in with a sealed session', async () => {
    const r = await get('/auth/callback?code=good-code&state=s1', { cookie: `${auth.STATE_COOKIE}=s1` });
    assert.equal(r.status, 302);
    assert.equal(r.headers.location, '/');
    const session = r.headers['set-cookie'].find((c) => c.startsWith('dash_session='));
    assert.match(session, /; Path=\/; HttpOnly; SameSite=Lax; Max-Age=604800$/);
    const value = decodeURIComponent(session.split(';')[0].slice('dash_session='.length));
    const payload = auth.unsealSession(value, KEY);
    assert.equal(payload.uid, USER_ID);
    assert.equal(payload.accessToken, TOKEN);
    assert.ok(payload.exp <= Date.now() + 7 * 24 * 3600 * 1000);
    const me = await get('/api/me', { cookie: `dash_session=${value}` });
    assert.equal(me.status, 200);
    assert.ok(log.lines.some((l) => l.includes('Dashboard login') && l.includes(USER_ID)));
  });

  test('POST /auth/logout clears the session and revokes the token', async () => {
    assert.equal((await call(port, 'POST', '/auth/logout', { headers: { cookie: sessionCookie() } })).status, 415);
    assert.equal((await get('/auth/logout')).status, 405);
    const r = await call(port, 'POST', '/auth/logout', { headers: { cookie: sessionCookie(), ...WRITE_HEADERS }, body: '{}' });
    assert.equal(r.status, 204);
    assert.equal(r.headers['content-length'], undefined, 'a 204 carries no Content-Length');
    assert.equal(r.text, '');
    assert.match(r.headers['set-cookie'][0], /^dash_session=; .*Max-Age=0/);
    await new Promise((res) => setImmediate(res));
    assert.deepEqual(fakes.calls.revokeToken, [TOKEN]);
    assert.deepEqual(fakes.calls.forgetUser, [USER_ID]);
  });

  test('logs never contain tokens or secrets', () => {
    for (const line of log.lines) {
      assert.equal(line.includes(TOKEN), false, line);
      assert.equal(line.includes(SECRET), false, line);
      assert.equal(line.includes('good-code'), false, line);
    }
  });
});

describe('HTTP server over https', () => {
  let server;
  let port;
  before(async () => {
    const { config } = loadDashboardConfig(baseEnv({ DASHBOARD_URL: 'https://dash.example.com' }));
    const fakes = createFakes();
    server = createDashboardServer({ config, deps: { discord: fakes.discord, settings: fakes.settings }, logger: recordingLogger() });
    port = await listen(server);
  });
  after(() => close(server));

  test('sends HSTS and Secure cookies, and checks the public origin', async () => {
    const health = await call(port, 'GET', '/healthz');
    assert.match(health.headers['strict-transport-security'], /max-age=\d+/);
    const login = await call(port, 'GET', '/auth/login');
    assert.match(login.headers['set-cookie'][0], /; Secure$/);
    const r = await call(port, 'PATCH', `/api/guilds/${G.zeta}/settings`, {
      headers: { cookie: sessionCookie(), ...WRITE_HEADERS, origin: 'http://localhost:8080' },
      body: JSON.stringify({ changes: { modEnabled: true } }),
    });
    assert.equal(r.status, 403, 'the origin must be the https DASHBOARD_URL');
    assert.match(r.json.message, /configured for https:\/\/dash\.example\.com\. Open it there, or set DASHBOARD_URL/);
    assert.equal(r.text.includes('localhost'), false, 'the request origin is not echoed');
  });
});

describe('parked server', () => {
  let server;
  let port;
  before(async () => {
    server = createParkedServer({ missing: ['DISCORD_CLIENT_SECRET', 'DASHBOARD_SESSION_SECRET'], invalid: [{ key: 'DASHBOARD_URL', reason: 'x' }] });
    port = await listen(server);
  });
  after(() => close(server));

  test('/healthz stays 200 and everything else names the missing keys', async () => {
    const health = await call(port, 'GET', '/healthz');
    assert.equal(health.status, 200);
    assert.equal(health.json.ok, true);
    const page = await call(port, 'GET', '/');
    assert.equal(page.status, 503);
    assert.match(page.text, /missing DISCORD_CLIENT_SECRET, DASHBOARD_SESSION_SECRET; invalid DASHBOARD_URL/);
    const api = await call(port, 'GET', '/api/me');
    assert.equal(api.status, 503);
    assert.equal(api.json.error, 'not_configured');
    assert.equal(api.headers['x-content-type-options'], 'nosniff');
  });
});

test('createRateLimiter allows `max` hits per window', () => {
  let t = 0;
  const hit = createRateLimiter({ max: 2, windowMs: 1_000, now: () => t });
  assert.equal(hit('a').ok, true);
  assert.equal(hit('a').ok, true);
  assert.deepEqual(hit('a'), { ok: false, retryAfter: 1 });
  assert.equal(hit('b').ok, true);
  t = 1_001;
  assert.equal(hit('a').ok, true);
});
