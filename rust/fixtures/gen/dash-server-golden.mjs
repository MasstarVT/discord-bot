// Goldens for rust/crates/dash-server, generated from the JS dashboard (run from the repository root:
// `node rust/fixtures/gen/dash-server-golden.mjs`). Writes rust/fixtures/golden/dash-server/:
//   http.json         scripted HTTP sessions against createDashboardServer (fake Discord, settings, records and bot-join
//                     dependencies ported one to one from test/dashboard.test.js, test/sec-dashboard.test.js and
//                     test/review-dashboard-server.test.js), with every answer (status, headers, body), the log lines and
//                     the calls the fakes saw. The Rust test replays the same steps against its Router.
//   discord-api.json  scripted Discord REST answers and the results of createDiscordApi's calls over them (the Rust test
//                     serves the same answers from a local stub).
//   pages.json        landing.js renders of the real templates and CHANGELOG.md for several option sets.
//   collator.json     Intl.Collator('en', { sensitivity: 'base', numeric: true }) on a table of names.
//   public-test/      the small public directory of the HTTP tests.
import { request } from 'node:http';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const OUT = join(ROOT, 'rust/fixtures/golden/dash-server');
const PUBLIC_TEST = join(OUT, 'public-test');

const realConsole = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const auth = await import('../../../src/dashboard/auth.js');
const { loadDashboardConfig } = await import('../../../src/dashboard/config.js');
const { createDashboardServer, createParkedServer, BODY_LIMIT_BYTES, RECORD_BODY_LIMIT_BYTES } = await import('../../../src/dashboard/server.js');
const discordApi = await import('../../../src/dashboard/discordApi.js');
const fields = await import('../../../src/dashboard/fields.js');
const schema = await import('../../../src/dashboard/schema.js');
const { COLLECTION_LIST } = await import('../../../src/dashboard/collections/index.js');
const { RecordError, withMessage } = await import('../../../src/dashboard/collections/errors.js');
const landing = await import('../../../src/dashboard/landing.js');
const { PermissionFlagsBits } = await import('discord.js');
const { DiscordApiError } = discordApi;

// ── Shared fixtures (test/dashboard.test.js) ─────────────────────────────────────────────────────────────────────

const NOW = 1_800_000_000_000;
const SECRET = 'x'.repeat(48);
const KEY = auth.deriveSessionKey(SECRET);
const USER_ID = '111111111111111111';
const OWNER_ID = '999999999999999999';
const TOKEN = 'user-access-token-abc123';
const G = Object.freeze({
  zeta: '200000000000000001', alpha: '200000000000000002', beta: '200000000000000003', plain: '200000000000000004',
  broken: '200000000000000005', stranger: '200000000000000099', botOnly: '200000000000000006',
});
const baseEnv = (extra = {}) => ({
  CLIENT_ID: '123456789012345678', DISCORD_TOKEN: 'bot-token', DISCORD_CLIENT_SECRET: 'client-secret',
  DASHBOARD_SESSION_SECRET: SECRET, DATABASE_URL: 'postgresql://u:p@localhost/db', REDIS_URL: 'redis://localhost:6379', ...extra,
});
const USER_GUILDS = Object.freeze([
  { id: G.zeta, name: 'zeta', icon: null, owner: false, permissions: '32' },
  { id: G.alpha, name: 'Alpha', icon: 'b'.repeat(32), owner: true, permissions: '0' },
  { id: G.beta, name: 'beta', icon: null, owner: false, permissions: '8' },
  { id: G.plain, name: 'Plain', icon: null, owner: false, permissions: '1024' },
  { id: G.broken, name: 'Broken', icon: null, owner: true, permissions: '0' },
]);
const BOT_GUILDS = new Set([G.zeta, G.beta, G.plain, G.broken, G.botOnly]);
const BOT_GUILD_ENTRIES = Object.freeze([...BOT_GUILDS].map((id) => {
  const g = USER_GUILDS.find((u) => u.id === id);
  return { id, name: g ? g.name : 'Bot only', icon: g ? g.icon : null };
}));
const CH = Object.freeze({
  text: '300000000000000001', voice: '300000000000000002', category: '300000000000000003', news: '300000000000000004',
  stale: '300000000000000098', ghost: '300000000000000099',
});
const RL = Object.freeze({
  low: '400000000000000001', admin: '400000000000000002', mid: '400000000000000003', managed: '400000000000000004',
  high: '400000000000000005', stale: '400000000000000098', ghost: '400000000000000099',
});
const BOT_ID = '123456789012345678';
const GUILD_OWNER = '555555555555555555';

function fakeResources(guildId) {
  const role = (id, name, position, extra = {}) => ({
    id, name, color: null, position, managed: false, everyone: false, mentionable: false, assignable: position < 6, dangerous: false, ...extra,
  });
  return {
    guild: { id: guildId, name: 'zeta', icon: null, memberCount: 12, ownerId: GUILD_OWNER },
    channels: [
      { id: CH.text, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false },
      { id: CH.voice, name: 'Lounge', type: 2, kind: 'voice', parentId: null, position: 1, nsfw: false },
      { id: CH.category, name: 'INFO', type: 4, kind: 'category', parentId: null, position: 0, nsfw: false },
      { id: CH.news, name: 'news', type: 5, kind: 'announcement', parentId: CH.category, position: 0, nsfw: false },
    ],
    roles: [
      role(RL.high, 'Above the bot', 8),
      role(RL.managed, 'Some bot', 5, { managed: true, assignable: false }),
      role(RL.mid, 'Mid', 3),
      role(RL.admin, 'Admins', 2, { dangerous: true }),
      role(RL.low, 'Low', 1),
      role(guildId, '@everyone', 0, { everyone: true, assignable: false }),
    ],
    emojis: [{ id: '600000000000000001', name: 'coin', animated: false }],
    bot: { userId: BOT_ID, topRolePosition: 6, manageRoles: true, createInvite: guildId !== G.plain },
  };
}

function createFakes() {
  const calls = { writeSettings: [], exchangeCode: [], revokeToken: [], forgetUser: [], getUserGuilds: [], resources: [], getMember: [], invites: [] };
  const inviteDeny = new Set([CH.text]);
  let row = {
    guildId: G.zeta, modEnabled: true, xpEnabled: false, voiceXpEnabled: true, levelingEnabled: true,
    captchaLockdownPrior: true, captchaMessageId: '700000000000000001', timezone: 'Europe/Berlin', customPrefix: '', embedColor: 0x5865f2,
    logIgnoredChannelIds: [CH.stale], modRoleIds: [RL.stale], xpMin: 15, xpMax: 25,
  };
  const discord = {
    async getUserGuilds(uid, token, opts = {}) {
      calls.getUserGuilds.push({ token, allowStale: opts?.allowStale === true });
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
    async getBotGuilds() { return BOT_GUILD_ENTRIES; },
    async createInvite(channelId, opts) {
      calls.invites.push({ channelId, reason: opts?.reason ?? '' });
      if (inviteDeny.has(channelId)) throw new DiscordApiError('Discord returned HTTP 403', { status: 403, code: 'forbidden', discordCode: 50013 });
      return { code: 'abc123', url: 'https://discord.gg/abc123', expiresAt: '2026-10-13T12:30:00.000Z' };
    },
    inviteUrl: (guildId) => `https://discord.com/oauth2/authorize?client_id=123456789012345678&guild_id=${guildId}`,
    async exchangeCode(code) {
      calls.exchangeCode.push(code);
      if (code !== 'good-code') throw new Error('token exchange returned HTTP 400');
      return { accessToken: TOKEN, expiresIn: 604_800 };
    },
    async getCurrentUser() { return { id: USER_ID, username: 'alice', globalName: 'Alice', avatar: null }; },
    async revokeToken(token) { calls.revokeToken.push(token); return true; },
    forgetUser(uid) { calls.forgetUser.push(uid); },
    async getGuildResources(guildId, opts = {}) {
      calls.resources.push({ guildId, fresh: opts.fresh === true });
      if (guildId === G.beta) throw new DiscordApiError('Discord returned HTTP 503', { status: 503, code: 'unavailable' });
      return fakeResources(guildId);
    },
    async getMember(guildId, userId) {
      calls.getMember.push({ guildId, userId });
      return { roles: [RL.low, RL.mid] };
    },
    async searchMembers(guildId, query) { return [{ id: USER_ID, username: 'alice', globalName: 'Alice', nick: query, avatarUrl: discordApi.userAvatarUrl({ id: USER_ID }), bot: false }]; },
  };
  const settings = {
    async readGuildSettings(guildId) {
      if (guildId === G.broken) throw new Error('database down');
      return { ...row, guildId };
    },
    async writeSettings(guildId, changes, ctx) {
      calls.writeSettings.push({ guildId, changes, userId: ctx.userId, hasDiscord: ctx.discord === discord });
      if (guildId === G.broken) throw new Error('database down');
      const data = fields.toStoredChanges(changes, row);
      const problem = fields.checkCrossFields({ ...row, ...data }, changes);
      if (problem) throw new schema.FieldError(problem.path, problem.message);
      row = { ...row, ...data };
      return { row: { ...row, guildId }, notices: Object.hasOwn(changes, 'economyMaxBalance') ? ['Lowered 2 balances to the new maximum.'] : [] };
    },
  };
  return { discord, settings, calls, inviteDeny };
}

function recordingLogger() {
  const lines = [];
  const rec = (level) => (...args) => lines.push(`${level} ${args.map((a) => (a instanceof Error ? a.message : String(a))).join(' ')}`);
  return { lines, info: rec('info'), warn: rec('warn'), error: rec('error'), debug: rec('debug'), success: rec('success') };
}

// validateRecordInput of src/dashboard/collections/index.js over an injected registry (the server's deps.collections).
const FORBIDDEN_KEYS = new Set(['__proto__', 'constructor', 'prototype']);
function validatorFor(COLLECTIONS) {
  return (key, data, { mode, actionId, current = null }) => {
    const c = COLLECTIONS[key];
    if (!c) throw new RecordError('not_found', 'Unknown list.');
    if (!schema.isPlainObject(data)) throw new schema.FieldError('', '"data" must be an object.');
    const action = mode === 'action' ? (c.actions ?? []).find((a) => a.id === actionId) : null;
    if (mode === 'action' && !action) throw new RecordError('not_found', 'Unknown action.');
    const list = action ? action.fields ?? [] : c.fields;
    const known = new Map(list.map((f) => [f.field, f]));
    for (const k of Object.keys(data)) {
      if (FORBIDDEN_KEYS.has(k) || !known.has(k)) throw new schema.FieldError(k.slice(0, 40), `"${k.slice(0, 40)}" is not something you can set here.`);
      if (mode === 'update' && (c.createOnly ?? []).includes(k)) throw new schema.FieldError(k, `"${known.get(k).label}" cannot be changed after creation.`);
    }
    const refs = schema.emptyRefs();
    const out = {};
    for (const f of list) {
      if (Object.hasOwn(data, f.field)) out[f.field] = schema.validateValue(f, data[f.field], f.field, refs);
      else if (mode !== 'update') {
        if (f.required) throw new schema.FieldError(f.field, `"${f.label}" is required.`);
        out[f.field] = f.default === undefined ? null : structuredClone(f.default);
      }
    }
    const check = action ? action.check : c.check;
    if (typeof check === 'function') {
      const problem = action ? check(out, { current }) : check(out, { mode, current });
      if (problem) throw typeof problem === 'object' ? new schema.FieldError(problem.path ?? '', problem.message) : new schema.FieldError('', problem);
    }
    return { data: out, refs };
  };
}

const THINGS = {
  key: 'things', module: 'embeds', label: 'Things', singular: 'thing', description: '', limit: 5, titleField: 'name',
  fields: [
    { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
    { field: 'channelId', type: 'channel', label: 'Channel', channelTypes: ['text'], nullable: true, default: null },
    { field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null },
  ],
  createOnly: ['name'],
  readOnly: [],
  actions: [{ id: 'post', label: 'Post', fields: [{ field: 'channelId', type: 'channel', label: 'Channel', channelTypes: ['text'], required: true }] }],
  check: (data, { mode, current }) => (mode === 'update' && current?.name === 'Locked' ? { path: 'channelId', message: 'Locked things keep their channel.' } : null),
};

function createRecordFakes() {
  const calls = [];
  const items = new Map([
    ['t1', { id: 't1', name: 'One', channelId: CH.stale, roleId: null }],
    ['t3', { id: 't3', name: 'Locked', channelId: CH.text, roleId: null }],
  ]);
  const records = {
    async list(guildId, key, opts) { calls.push(['list', guildId, key, opts]); return [...items.values()]; },
    async get(guildId, key, id) { calls.push(['get', guildId, key, id]); return items.get(id) ?? null; },
    async create(guildId, key, data, actor) {
      calls.push(['create', guildId, key, data, actor]);
      if (data.name === 'taken') throw new RecordError('conflict', 'A thing with that name exists.', { path: 'name' });
      if (data.name === 'full') throw new RecordError('limit', 'This server already has 5 things.');
      if (data.name === 'boom') throw new Error('database down');
      if (data.name === 'half') throw new RecordError('failed', 'The thing was saved, but it was not posted.', { saved: true });
      if (data.name === 'noted') return withMessage({ id: 't5', ...data }, 'Saved with a note.');
      if (data.name === 'refused') throw new DiscordApiError('Discord returned HTTP 403', { status: 403, code: 'forbidden', discordCode: 50013 });
      const item = { id: 't2', ...data };
      items.set('t2', item);
      return item;
    },
    async update(guildId, key, id, data, actor) {
      calls.push(['update', guildId, key, id, data, actor]);
      const item = { ...items.get(id), ...data };
      items.set(id, item);
      return item;
    },
    async remove(guildId, key, id, actor) {
      calls.push(['remove', guildId, key, id, actor]);
      if (!items.delete(id)) throw new RecordError('not_found', 'That thing does not exist any more.');
    },
    async action(guildId, key, id, actionId, data, actor) {
      calls.push(['action', guildId, key, id, actionId, data, actor]);
      if (id === 't3') throw new RecordError('discord', 'The bot cannot post in that channel.');
      return { item: items.get(id), message: 'Posted.' };
    },
  };
  return { records, calls, items };
}

// ── HTTP plumbing ────────────────────────────────────────────────────────────────────────────────────────────────

function call(port, method, path, { headers = {}, body } = {}) {
  return new Promise((resolvePromise, reject) => {
    const framed = body === undefined || Object.keys(headers).some((h) => h.toLowerCase() === 'content-length')
      ? headers : { ...headers, 'content-length': Buffer.byteLength(body) };
    const req = request({ host: '127.0.0.1', port, method, path, headers: framed, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolvePromise({ status: res.statusCode, headers: res.headers, raw: Buffer.concat(chunks) }));
    });
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}
const listen = (server) => new Promise((res) => server.listen(0, '127.0.0.1', () => res(server.address().port)));
const close = (server) => new Promise((res) => { server.close(() => res()); server.closeAllConnections(); });

const KEEP_HEADERS = ['content-type', 'cache-control', 'retry-after', 'allow', 'location', 'set-cookie', 'content-encoding', 'vary', 'etag',
  'content-security-policy', 'x-content-type-options', 'referrer-policy', 'x-frame-options', 'cross-origin-opener-policy',
  'permissions-policy', 'strict-transport-security'];

/** One answer as the golden keeps it: random parts (OAuth state, sealed cookies) normalised, a sealed session opened. */
function normalize(r) {
  const headers = {};
  let session;
  for (const k of KEEP_HEADERS) {
    let v = r.headers[k];
    if (v === undefined) continue;
    if (k === 'set-cookie') {
      v = v.map((c) => {
        const m = /^dash_session=(v1\.[^;]+);/.exec(c);
        if (m) session = auth.unsealSession(decodeURIComponent(m[1]), KEY, NOW);
        return c.replace(/^dash_oauth_state=[A-Za-z0-9_-]{43};/, 'dash_oauth_state=<state>;').replace(/^dash_session=v1\.[^;]+;/, 'dash_session=<sealed>;');
      });
    }
    if (k === 'location') v = v.replace(/state=[A-Za-z0-9_-]{43}/, 'state=<state>');
    headers[k] = v;
  }
  const out = { status: r.status, headers, hasContentLength: r.headers['content-length'] !== undefined };
  let raw = r.raw;
  if (r.headers['content-encoding'] === 'gzip' && raw.length > 0) raw = gunzipSync(raw);
  const type = r.headers['content-type'] ?? '';
  if (/^application\/json/.test(type)) {
    try { out.json = JSON.parse(raw.toString('utf8')); } catch { out.text = raw.toString('utf8'); }
  } else if (/^(text\/|image\/svg)/.test(type)) {
    out.text = raw.toString('utf8');
  } else {
    out.sha256 = createHash('sha256').update(raw).digest('hex');
    out.length = raw.length;
  }
  if (session !== undefined) out.session = session;
  return out;
}

const sessionCookie = (overrides = {}) => `${auth.SESSION_COOKIE}=${auth.sealSession({
  uid: USER_ID, username: 'alice', globalName: 'Alice', avatar: null, accessToken: TOKEN, exp: NOW + 3_600_000, ...overrides,
}, KEY)}`;
const WRITE_HEADERS = Object.freeze({ 'content-type': 'application/json', 'x-dashboard': '1', origin: 'http://localhost:8080' });

// Steps: { method, path, headers?, body?, bodyGen?, cookieFromPrevious? } or { advance: ms } or { fake: name } or { wait: ms }.
const req = (method, path, headers = {}, body) => ({ method, path, headers, ...(body === undefined ? {} : { body }) });
const get = (path, headers = {}) => req('GET', path, headers);
const json = (b) => (typeof b === 'string' ? b : JSON.stringify(b));

function bodyOf(step) {
  if (step.bodyGen?.kind === 'settingsPad') return JSON.stringify({ changes: { modEnabled: true }, pad: 'x'.repeat(step.bodyGen.n) });
  if (step.bodyGen?.kind === 'recordName') return JSON.stringify({ data: { name: 'x'.repeat(step.bodyGen.n) } });
  return step.body;
}

async function runScenario({ name, server: make, steps, fakes }) {
  const clock = { t: NOW };
  const log = recordingLogger();
  const server = make({ log, clock });
  const port = await listen(server);
  const responses = [];
  let lastSession;
  for (const step of steps) {
    if (step.advance !== undefined) { clock.t += step.advance; continue; }
    if (step.wait !== undefined) { await new Promise((res) => setTimeout(res, step.wait)); continue; }
    if (step.fake !== undefined) { fakes?.act(step.fake); continue; }
    const headers = { ...step.headers };
    if (step.cookieFromPrevious) headers.cookie = `dash_session=${lastSession}`;
    const r = await call(port, step.method, step.path, { headers, body: bodyOf(step) });
    for (const c of r.headers['set-cookie'] ?? []) {
      const m = /^dash_session=(v1\.[^;]+);/.exec(c);
      if (m) lastSession = decodeURIComponent(m[1]);
    }
    responses.push(normalize(r));
  }
  await close(server);
  return { name, steps, responses, log: log.lines, calls: fakes?.calls() ?? null };
}

// ── Scenarios ────────────────────────────────────────────────────────────────────────────────────────────────────

const STYLES = 'body { color: red; }\n'.repeat(200);
function writePublicTest() {
  rmSync(PUBLIC_TEST, { recursive: true, force: true });
  mkdirSync(join(PUBLIC_TEST, 'demo'), { recursive: true });
  writeFileSync(join(PUBLIC_TEST, 'index.html'), '<!doctype html><title>t</title>');
  writeFileSync(join(PUBLIC_TEST, 'landing.html'), '<!doctype html><title>l</title><p><!--module-count--> <!--code:leveling--> <a href="@@INVITE_URL@@">Add</a></p>');
  writeFileSync(join(PUBLIC_TEST, 'app.js'), 'export {};');
  writeFileSync(join(PUBLIC_TEST, 'styles.css'), STYLES);
  writeFileSync(join(PUBLIC_TEST, 'privacy.html'), '<!doctype html><title>p</title><p>Ask <!--contact-->.</p><p><!--module-count--> <a href="@@INVITE_URL@@">Add</a></p>');
  writeFileSync(join(PUBLIC_TEST, 'docs.html'), '<!doctype html><title>d</title><!--docs-modules--><!--docs-core-->');
  writeFileSync(join(PUBLIC_TEST, 'changelog.html'), '<!doctype html><title>c</title><!--changelog-->');
  writeFileSync(join(PUBLIC_TEST, 'forms.css'), '<!--nope-->');
  writeFileSync(join(PUBLIC_TEST, 'terms.html'), '<!doctype html><title>t</title><!--bogus-marker-->');
  writeFileSync(join(PUBLIC_TEST, 'demo', 'rank-card.png'), Buffer.from([0x89, 0x50, 0x4e, 0x47]));
}
const etagOf = (text) => `"${createHash('sha256').update(Buffer.from(text, 'utf8')).digest('base64url').slice(0, 27)}"`;

function fakesHandle(f) {
  return {
    act(name) {
      if (name === 'denyAll') for (const id of [CH.text, CH.news, CH.voice]) f.inviteDeny.add(id);
      if (name === 'restoreDeny') { f.inviteDeny.clear(); f.inviteDeny.add(CH.text); }
    },
    calls: () => JSON.parse(JSON.stringify(f.calls)),
  };
}

function mainScenario() {
  const f = createFakes();
  const { config } = loadDashboardConfig(baseEnv({ BOT_OWNER_ID: OWNER_ID }));
  const S = (o) => sessionCookie(o);
  const authed = (path, extra = {}) => get(path, { cookie: S(), ...extra });
  const patch = (gid, body, headers = {}) => req('PATCH', `/api/guilds/${gid}/settings`, { cookie: S(), ...WRITE_HEADERS, ...headers }, json(body));
  const steps = [];
  const add = (...s) => steps.push(...s);
  // healthz
  add(get('/healthz'), req('POST', '/healthz'), req('HEAD', '/healthz'));
  // me
  add(get('/api/me'), get('/api/me', { cookie: `${auth.SESSION_COOKIE}=v1.forged` }), get('/api/me', { cookie: S({ exp: NOW - 1 }) }),
    get('/api/me', { cookie: S({ uid: 'x' }) }), get('/api/me', { cookie: S({ globalName: undefined }) }), authed('/api/me'),
    get('/api/me', { cookie: S({ avatar: 'a'.repeat(32) }) }));
  // catalog
  add(get('/api/catalog'), authed('/api/catalog'));
  // guilds
  add(authed('/api/guilds'));
  for (const id of [G.plain, G.botOnly, G.stranger]) {
    for (const path of [`/api/guilds/${id}/settings`, `/api/guilds/${id}/resources`, `/api/guilds/${id}/members?query=a`]) add(authed(path));
  }
  const owner = S({ uid: OWNER_ID });
  add(authed('/api/guilds', { cookie: owner }), authed(`/api/guilds/${G.plain}/settings`, { cookie: owner }),
    authed(`/api/guilds/${G.botOnly}/settings`, { cookie: owner }), authed(`/api/guilds/${G.stranger}/settings`, { cookie: owner }));
  add(authed('/api/guilds', { cookie: S({ accessToken: 'expired-token' }) }), authed('/api/guilds', { cookie: S({ accessToken: 'limited-token' }) }),
    authed('/api/guilds', { cookie: S({ accessToken: 'down-token' }) }));
  for (const accessToken of ['stale-limited-token', 'stale-down-token']) {
    const cookie = S({ uid: '444444444444444444', accessToken });
    add(authed('/api/guilds', { cookie }), authed(`/api/guilds/${G.zeta}/settings`, { cookie }), patch(G.zeta, { changes: { modEnabled: false } }, { cookie }));
  }
  // GET settings
  add(authed(`/api/guilds/${G.plain}/settings`), authed(`/api/guilds/${G.stranger}/settings`), get(`/api/guilds/${G.zeta}/settings`),
    authed('/api/guilds/abc/settings'), authed(`/api/guilds/${G.alpha}/settings`), authed(`/api/guilds/${G.zeta}/settings`),
    authed(`/api/guilds/${G.broken}/settings`), req('DELETE', `/api/guilds/${G.zeta}/settings`, { cookie: S() }));
  // PATCH CSRF
  const body = { changes: { modEnabled: false } };
  add(patch(G.zeta, body, { 'x-dashboard': '' }), patch(G.zeta, body, { origin: 'https://evil.example' }), patch(G.zeta, body, { origin: 'null' }),
    patch(G.zeta, body, { 'content-type': 'text/plain' }), patch(G.zeta, body, { 'sec-fetch-site': 'cross-site' }),
    patch(G.zeta, body, { 'sec-fetch-site': 'same-origin', 'content-type': 'application/json; charset=utf-8' }),
    req('PATCH', `/api/guilds/${G.zeta}/settings`, { ...WRITE_HEADERS }, json(body)));
  // bad bodies
  const badCookie = S({ uid: '777777777777777777' });
  for (const b of ['{', '', 'null', '[]', { changes: {} }, { changes: { modEnabled: 'false' } }, { changes: { levelingEnabled: true } },
    { changes: { nope: true } }, { changes: { captchaMessageId: '700000000000000002' } }, { changes: { automodConfig: {} } }, { changes: true }, { other: 1 }]) {
    add(patch(G.zeta, b, { cookie: badCookie }));
  }
  add({ ...patch(G.zeta, '', { cookie: badCookie }), body: undefined, bodyGen: { kind: 'settingsPad', n: BODY_LIMIT_BYTES } });
  // paths
  const pathCookie = S({ uid: '666666666666666666' });
  for (const changes of [{ xpMin: 0 }, { modEnabled: 'false' }, { welcomeCardStyle: { enabled: true, background: 'nope' } },
    { automodBadWords: ['fine word', '!'] }, { infractionEscalation: [{ warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: 60 }] }]) {
    add(patch(G.zeta, { changes }, { cookie: pathCookie }));
  }
  add(patch(G.plain, { changes: { modEnabled: false } }), patch(G.alpha, { changes: { modEnabled: false } }));
  add(patch(G.zeta, { changes: { voiceXpEnabled: false, modEnabled: false, timezone: 'europe/paris', welcomeMessage: 'Hi {user}!' } }),
    patch(G.zeta, { changes: { economyMaxBalance: 5000 } }), patch(G.broken, { changes: { modEnabled: true } }));
  // ids
  const idCookie = S({ uid: '700000000000000011' });
  for (const changes of [{ logIgnoredChannelIds: [CH.text, CH.ghost] }, { welcomeChannelId: CH.voice }, { statsCategoryId: CH.text },
    { modRoleIds: [G.zeta] }, { autoroleIds: [RL.low, RL.high] }, { autoroleIds: [RL.managed] }, { muteRoleId: RL.admin },
    { autoroleIds: [RL.mid] }, { automodRuleCaps: { channelIds: [CH.ghost] } },
    { logIgnoredChannelIds: [CH.stale, CH.text], modRoleIds: [RL.stale, RL.high], autoroleIds: [RL.low] }]) {
    add(patch(G.zeta, { changes }, { cookie: idCookie }));
  }
  add(patch(G.zeta, { changes: { autoroleIds: [RL.mid] } }, { cookie: S({ uid: GUILD_OWNER }) }));
  const crossCookie = S({ uid: '700000000000000012' });
  add(patch(G.zeta, { changes: { xpMin: 30 } }, { cookie: crossCookie }), patch(G.zeta, { changes: { xpMin: 30, xpMax: 40 } }, { cookie: crossCookie }),
    patch(G.zeta, { changes: { levelUpMode: 'CHANNEL' } }, { cookie: crossCookie }), patch(G.beta, { changes: { welcomeChannelId: CH.text } }, { cookie: crossCookie }));
  // resources
  const resCookie = S({ uid: '700000000000000013' });
  add(get(`/api/guilds/${G.zeta}/resources`), authed(`/api/guilds/${G.plain}/resources`, { cookie: resCookie }),
    authed(`/api/guilds/${G.alpha}/resources`, { cookie: resCookie }), authed('/api/guilds/x/resources', { cookie: resCookie }),
    authed(`/api/guilds/${G.beta}/resources`, { cookie: resCookie }), authed(`/api/guilds/${G.zeta}/resources`, { cookie: resCookie }),
    authed(`/api/guilds/${G.zeta}/resources?fresh=1`, { cookie: resCookie }),
    req('POST', `/api/guilds/${G.zeta}/resources`, { cookie: resCookie, ...WRITE_HEADERS }, '{}'));
  // members
  add(authed(`/api/guilds/${G.zeta}/members`), authed(`/api/guilds/${G.zeta}/members?query=%01%20%20`), authed(`/api/guilds/${G.zeta}/members?query=al+ice`));
  // rate limit
  const rlCookie = S({ uid: '333333333333333333' });
  for (let i = 0; i < 21; i++) add(patch(G.zeta, '{', { cookie: rlCookie }));
  add(patch(G.zeta, '{'));
  // a handler that throws
  add(authed('/api/guilds', { cookie: S({ accessToken: 'broken-token' }) }), get('/healthz'));
  // static
  add(get('/'), get('/docs'), get('/changelog'), get('/demo/rank-card.png'), get('/dashboard'), get('/dashboard?error=login_failed'),
    get('/index.html'), get('/dashboard/'), get('/app.js'), get('/favicon.svg'), req('POST', '/app.js'), req('HEAD', '/app.js'), get('/forms.css'));
  for (const path of ['/../package.json', '/%2e%2e/package.json', '/..%2fpackage.json', '/package.json', '/src/dashboard/server.js',
    '/public/app.js', '/index.html/', '/app.js%00', '/api/nope', '/API/me']) add(get(path));
  add(get('/privacy'), get('/terms'), get('/privacy.html'));
  const cssTag = etagOf(STYLES);
  const gzTag = `${cssTag.slice(0, -1)}-gz"`;
  add(get('/styles.css'), get('/styles.css', { 'accept-encoding': 'br, gzip;q=0.8' }), get('/styles.css', { 'accept-encoding': 'gzip;q=0' }),
    get('/styles.css', { 'accept-encoding': 'GZIP' }), get('/styles.css', { 'accept-encoding': 'deflate, gzip;q=0.0' }));
  for (const tag of [cssTag, `W/${cssTag}`, `"other", ${gzTag}`, '*', '"stale"', '', ' ']) add(get('/styles.css', { 'if-none-match': tag }));
  add(get('/dashboard', { 'accept-encoding': 'gzip' }), req('HEAD', '/styles.css'));
  // login
  add(get('/auth/login'), req('POST', '/auth/login'));
  for (const [path, cookie] of [
    ['/auth/callback?code=good-code&state=abc', `${auth.STATE_COOKIE}=different`],
    ['/auth/callback?code=good-code&state=abc', undefined],
    ['/auth/callback?code=good-code', `${auth.STATE_COOKIE}=abc`],
    ['/auth/callback?error=access_denied&state=abc', `${auth.STATE_COOKIE}=abc`],
    ['/auth/callback?error=%0Abad%01&state=abc', `${auth.STATE_COOKIE}=abc`],
    ['/auth/callback?state=abc', `${auth.STATE_COOKIE}=abc`],
    ['/auth/callback?state=abc&code=no%20good', `${auth.STATE_COOKIE}=abc`],
  ]) add(get(path, cookie ? { cookie } : {}));
  add(get('/auth/callback?code=bad-code&state=abc', { cookie: `${auth.STATE_COOKIE}=abc` }));
  add(get('/auth/callback?code=good-code&state=s1', { cookie: `${auth.STATE_COOKIE}=s1` }), { ...get('/api/me'), cookieFromPrevious: true });
  // logout
  add(req('POST', '/auth/logout', { cookie: S() }), get('/auth/logout'), req('POST', '/auth/logout', { cookie: S(), ...WRITE_HEADERS }, '{}'), { wait: 20 },
    req('POST', '/auth/logout', { ...WRITE_HEADERS }, '{}'), req('POST', '/auth/logout', { cookie: `${auth.SESSION_COOKIE}=junk`, ...WRITE_HEADERS }, '{}'));
  // invite
  const invite = (gid, cookie = owner, headers = WRITE_HEADERS) => req('POST', `/api/guilds/${gid}/invite`, { cookie, ...headers }, '{}');
  add(invite(G.botOnly));
  for (const id of [G.zeta, G.botOnly, G.stranger]) add(invite(id, S()));
  add(invite(G.botOnly, owner, { ...WRITE_HEADERS, 'x-dashboard': '0' }), req('GET', `/api/guilds/${G.botOnly}/invite`, { cookie: owner }),
    invite('nope'), invite(G.stranger), invite(G.plain), { fake: 'denyAll' }, invite(G.botOnly), { fake: 'restoreDeny' }, invite(G.beta));
  // bot owner writes to a server they are not in
  const send = (changes) => patch(G.botOnly, { changes }, { cookie: owner });
  add(send({ autoroleIds: [] }), send({ autoroleIds: [RL.mid] }), send({ autoroleIds: [RL.high] }));
  // absolute-form and odd targets are not reachable through node:http's client; covered by the Rust socket tests.
  return {
    name: 'main', fakes: fakesHandle(f), steps,
    server: ({ log, clock }) => createDashboardServer({ config, deps: { discord: f.discord, settings: f.settings, now: () => clock.t }, logger: log, publicDir: PUBLIC_TEST }),
  };
}

function recordsScenario() {
  const f = createFakes();
  const rec = createRecordFakes();
  const { config } = loadDashboardConfig(baseEnv());
  const base = `/api/guilds/${G.zeta}/records/things`;
  const write = (method, path, body, { uid = '800000000000000001', headers = {} } = {}) => ({
    ...req(method, path, { cookie: sessionCookie({ uid }), ...WRITE_HEADERS, ...headers }, body === undefined ? undefined : json(body)),
  });
  const getAs = (path, uid = '800000000000000001') => get(path, { cookie: sessionCookie({ uid }) });
  const steps = [
    write('POST', base, { data: { name: 'half' } }), write('POST', base, { data: { name: 'full' } }),
    get(base), getAs(`/api/guilds/${G.plain}/records/things`), getAs(`/api/guilds/${G.alpha}/records/things`), getAs(base),
    getAs(`${base}?query=%00One&cursor=bad%20cursor`), getAs(`${base}?cursor=abc:1`), getAs(`${base}/t1`), getAs(`${base}/t9`),
    getAs(`/api/guilds/${G.zeta}/records/nope`), getAs(`${base}/bad.id`), getAs(`${base}/${'x'.repeat(41)}`),
    write('POST', `${base}/t1/actions/explode`, { data: {} }), write('POST', `${base}/t1/actions/9post`, { data: {} }),
    getAs('/api/guilds/x/records/things'), write('PUT', base, { data: {} }), write('POST', `${base}/t1`, { data: {} }),
    write('POST', base, { data: { name: 'Two', channelId: CH.text } }),
  ];
  const c2 = { uid: '800000000000000002' };
  for (const b of [{ data: { name: '' } }, { data: { channelId: CH.text } }, { data: { name: 'x', channelId: CH.ghost } },
    { data: { name: 'x', channelId: CH.voice } }, { data: { name: 'x', roleId: RL.admin } }, { data: { name: 'x', roleId: RL.high } },
    { data: { name: 'x', extra: 1 } }, { name: 'x' }, { data: { name: 'x' }, more: 1 }, '[]', '{', {}, { data: null }, 'null']) {
    steps.push(write('POST', base, b, c2));
  }
  steps.push(write('POST', base, { data: { name: 'taken' } }), write('POST', base, { data: { name: 'full' } }), write('POST', base, { data: { name: 'boom' } }),
    write('POST', base, { data: { name: 'noted' } }), write('POST', base, { data: { name: 'refused' } }));
  const c3 = { uid: '800000000000000003' };
  steps.push(write('PATCH', `${base}/t1`, { data: { channelId: CH.stale } }, c3), write('PATCH', `${base}/t1`, { data: { name: 'New' } }, c3),
    write('PATCH', `${base}/t3`, { data: { channelId: CH.text } }, c3), write('PATCH', `${base}/t9`, { data: { channelId: CH.text } }, c3));
  const c4 = { uid: '800000000000000004' };
  steps.push(write('DELETE', `${base}/t2`, '{}', c4), write('DELETE', `${base}/t2`, undefined, c4), write('DELETE', `${base}/t1`, '  ', c4),
    write('DELETE', `${base}/t1`, '[1', c4));
  const c5 = { uid: '800000000000000005' };
  steps.push(write('POST', `${base}/t3/actions/post`, { data: { channelId: CH.text } }, c5), write('POST', `${base}/t1/actions/post`, { data: {} }, c5),
    write('POST', `${base}/t9/actions/post`, { data: { channelId: CH.text } }, c5));
  steps.push(write('POST', base, { data: { name: 'x' } }, { headers: { 'x-dashboard': '' } }),
    write('DELETE', `${base}/t1`, '{}', { headers: { origin: 'https://evil.example' } }),
    write('PATCH', `${base}/t1`, { data: {} }, { headers: { 'content-type': 'text/plain' } }));
  steps.push({ ...write('POST', base, undefined, { uid: '800000000000000006' }), bodyGen: { kind: 'recordName', n: RECORD_BODY_LIMIT_BYTES } });
  for (let i = 0; i < 21; i++) steps.push(write('POST', base, '{', { uid: '800000000000000007' }));
  return {
    name: 'records',
    fakes: { act() {}, calls: () => JSON.parse(JSON.stringify(rec.calls)) },
    steps,
    server: ({ log, clock }) => createDashboardServer({
      config, logger: log,
      deps: { discord: f.discord, settings: f.settings, records: rec.records, now: () => clock.t,
        collections: { COLLECTIONS: { things: THINGS }, validateRecordInput: validatorFor({ things: THINGS }) } },
    }),
  };
}

function registryScenarios() {
  const f = createFakes();
  const rec = createRecordFakes();
  const { config } = loadDashboardConfig(baseEnv());
  const cookie = sessionCookie({ uid: '800000000000000008' });
  const steps = COLLECTION_LIST.map((col) => get(`/api/guilds/${G.zeta}/records/${col.key}`, { cookie }));
  steps.push(get(`/api/guilds/${G.zeta}/records/things`, { cookie }), get(`/api/guilds/${G.zeta}/records/constructor`, { cookie }),
    get(`/api/guilds/${G.zeta}/records/__proto__`, { cookie }));
  const real = {
    name: 'registry', steps,
    server: ({ log, clock }) => createDashboardServer({ config, logger: log, deps: { discord: f.discord, settings: f.settings, records: rec.records, now: () => clock.t } }),
  };
  const bare = {
    name: 'bare', steps: [get(`/api/guilds/${G.zeta}/records/${COLLECTION_LIST[0].key}`, { cookie }),
      req('POST', `/api/guilds/${G.zeta}/records/${COLLECTION_LIST[0].key}`, { cookie, ...WRITE_HEADERS }, '{"data":{}}')],
    server: ({ log, clock }) => createDashboardServer({ config, logger: log, deps: { discord: f.discord, settings: f.settings, now: () => clock.t } }),
  };
  return [real, bare];
}

const PERKS = {
  key: 'perks', module: 'embeds', label: 'Perks', singular: 'perk', description: '', limit: 5, titleField: 'name',
  fields: [
    { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
    { field: 'pingRoleIds', type: 'roles', label: 'Ping roles', maxItems: 5, default: [] },
    { field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null },
    { field: 'rewardRoleId', type: 'role', label: 'Reward', assignable: true, allowDangerous: true, nullable: true, default: null },
  ],
  createOnly: [], readOnly: [], actions: [],
};
const ACTS = {
  key: 'acts', module: 'embeds', label: 'Acts', singular: 'act', description: '', limit: 5, titleField: 'id',
  fields: [{
    field: 'actions', type: 'objectList', label: 'Actions', maxItems: 5, default: [],
    itemFields: [
      { field: 'type', type: 'enum', label: 'Type', options: [{ value: 'GIVE', label: 'Give' }, { value: 'TAKE', label: 'Take' }], default: 'GIVE' },
      { field: 'roleIds', type: 'roles', label: 'Roles', assignable: true, maxItems: 5, default: [] },
    ],
  }],
  createOnly: [], readOnly: [], actions: [],
  refKeys: (item, keys) => {
    const out = new Set([...keys].filter((k) => !k.startsWith('actions.roleIds|')));
    for (const a of item.actions ?? []) for (const id of a.roleIds ?? []) out.add(`actions.${a.type}|${id}`);
    return out;
  },
  refKeyOf: (ref, data) => {
    const m = /^actions\.(\d+)\.roleIds\.\d+$/.exec(ref.path);
    return m ? `actions.${data.actions[Number(m[1])].type}|${ref.id}` : schema.refKey(ref.path, ref.id);
  },
};
const LOCKS = {
  key: 'locks', module: 'embeds', label: 'Locks', singular: 'lock', description: '', limit: 5, titleField: 'name',
  fields: [
    { field: 'name', type: 'text', label: 'Name', maxLength: 32, minLength: 1, required: true },
    { field: 'enabled', type: 'boolean', label: 'Enabled', default: true },
    { field: 'actions', type: 'objectList', label: 'Actions', maxItems: 5, default: [],
      itemFields: [{ field: 'roleId', type: 'role', label: 'Role', assignable: true, nullable: true, default: null }] },
  ],
  createOnly: [], readOnly: [], actions: [], checkStoredRoles: true,
};

function rolesScenario() {
  const f = createFakes();
  const { config } = loadDashboardConfig(baseEnv());
  const MEMBER = '800000000000000021';
  const items = new Map([
    ['p1', { id: 'p1', name: 'One', pingRoleIds: [RL.admin, RL.mid], roleId: null, rewardRoleId: null }],
    ['a1', { id: 'a1', actions: [{ type: 'TAKE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] }],
    ['l-high', { id: 'l-high', name: 'High', enabled: false, actions: [{ roleId: RL.low }, { roleId: RL.mid }] }],
    ['l-low', { id: 'l-low', name: 'Low', enabled: false, actions: [{ roleId: RL.low }] }],
    ['n-high', { id: 'n-high', name: 'NoFlag', enabled: false, pingRoleIds: [], roleId: RL.mid, rewardRoleId: null }],
  ]);
  const records = {
    async get(guildId, key, id) { return items.get(id) ?? null; },
    async update(guildId, key, id, data) { const item = { ...items.get(id), ...data }; items.set(id, item); return item; },
  };
  const COLLECTIONS = { perks: PERKS, acts: ACTS, locks: LOCKS };
  const send = (method, path, body, uid = MEMBER) => req(method, path, { cookie: sessionCookie({ uid }), ...WRITE_HEADERS }, JSON.stringify(body));
  const sp = (changes, uid) => send('PATCH', `/api/guilds/${G.zeta}/settings`, { changes }, uid);
  const p1 = `/api/guilds/${G.zeta}/records/perks/p1`;
  const a1 = `/api/guilds/${G.zeta}/records/acts/a1`;
  const high = `/api/guilds/${G.zeta}/records/locks/l-high`;
  const low = `/api/guilds/${G.zeta}/records/locks/l-low`;
  const steps = [
    sp({ modRoleIds: [RL.admin, RL.mid] }, MEMBER), sp({ autoroleIds: [RL.admin] }, MEMBER), sp({ autoroleIds: [RL.mid] }, MEMBER),
    sp({ autoroleIds: [RL.mid] }, GUILD_OWNER), sp({ autoroleIds: [RL.mid, RL.low] }, MEMBER), sp({ muteRoleId: RL.mid }, MEMBER),
    send('PATCH', p1, { data: { roleId: RL.admin } }), send('PATCH', p1, { data: { roleId: RL.mid } }), send('PATCH', p1, { data: { rewardRoleId: RL.admin } }),
    send('PATCH', a1, { data: { actions: [{ type: 'TAKE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] } }),
    send('PATCH', a1, { data: { actions: [{ type: 'GIVE', roleIds: [RL.mid] }, { type: 'GIVE', roleIds: [RL.admin] }] } }),
    send('PATCH', high, { data: { enabled: true } }), send('PATCH', high, { data: { name: 'Renamed' } }),
    send('PATCH', high, { data: { actions: [{ roleId: RL.low }, { roleId: RL.mid }] } }), send('PATCH', high, { data: { actions: [{ roleId: RL.low }] } }),
    send('PATCH', low, { data: { enabled: true } }), { fake: 'resetHigh' }, send('PATCH', high, { data: { enabled: true } }, GUILD_OWNER),
    send('PATCH', `/api/guilds/${G.zeta}/records/perks/n-high`, { data: { name: 'Quiet' } }),
  ];
  return {
    name: 'roles', steps,
    fakes: {
      act(name) { if (name === 'resetHigh') items.set('l-high', { id: 'l-high', name: 'High', enabled: false, actions: [{ roleId: RL.mid }] }); },
      calls: () => JSON.parse(JSON.stringify([...items.values()])),
    },
    server: ({ log, clock }) => createDashboardServer({
      config, logger: log,
      deps: { discord: f.discord, settings: f.settings, records, now: () => clock.t, collections: { COLLECTIONS, validateRecordInput: validatorFor(COLLECTIONS) } },
    }),
  };
}

function httpsScenario() {
  const f = createFakes();
  const { config } = loadDashboardConfig(baseEnv({ DASHBOARD_URL: 'https://dash.example.com', DASHBOARD_CONTACT: 'owner@example.com', DASHBOARD_SUPPORT_URL: 'https://discord.gg/abc' }));
  return {
    name: 'https',
    steps: [get('/healthz'), get('/auth/login'), get('/api/me', { cookie: 'dash_session=v1.junk' }),
      req('PATCH', `/api/guilds/${G.zeta}/settings`, { cookie: sessionCookie(), ...WRITE_HEADERS, origin: 'http://localhost:8080' }, JSON.stringify({ changes: { modEnabled: true } })),
      req('PATCH', `/api/guilds/${G.zeta}/settings`, { cookie: sessionCookie(), ...WRITE_HEADERS, origin: 'https://dash.example.com' }, JSON.stringify({ changes: { modEnabled: true } })),
      get('/privacy'), get('/')],
    server: ({ log, clock }) => createDashboardServer({ config, deps: { discord: f.discord, settings: f.settings, now: () => clock.t }, logger: log, publicDir: PUBLIC_TEST }),
  };
}

function joinsScenario() {
  const f = createFakes();
  const { config } = loadDashboardConfig(baseEnv({ BOT_OWNER_ID: OWNER_ID }));
  const joinedAt = new Date('2026-10-01T12:00:00.000Z');
  const stored = new Map([
    [G.zeta, { inviterId: '200000000000000001', inviterName: 'alice', source: 'audit', joinedAt }],
    [G.botOnly, { inviterId: null, inviterName: null, source: null, joinedAt }],
  ]);
  let failing = false;
  const asked = [];
  const botJoins = {
    listBotJoins: async (ids) => {
      asked.push(ids);
      if (failing) throw new Error('database down');
      return new Map(ids.filter((id) => stored.has(id)).map((id) => [id, stored.get(id)]));
    },
  };
  const owner = sessionCookie({ uid: OWNER_ID });
  return {
    name: 'joins',
    steps: [get('/api/guilds', { cookie: sessionCookie() }), get('/api/guilds', { cookie: owner }), { fake: 'fail' }, get('/api/guilds', { cookie: owner })],
    fakes: { act() { failing = true; }, calls: () => JSON.parse(JSON.stringify(asked)) },
    server: ({ log, clock }) => createDashboardServer({ config, logger: log, deps: { discord: f.discord, settings: f.settings, botJoins, now: () => clock.t } }),
  };
}

function siteScenario() {
  const f = createFakes();
  const { config } = loadDashboardConfig(baseEnv());
  const paths = ['/', '/dashboard', '/index.html', '/landing.js', '/app.js', '/lib.js', '/forms.js', '/collections.js', '/embed-editor.js', '/tokens.css',
    '/styles.css', '/landing.css', '/site.css', '/leaflet.css', '/docs', '/changelog', '/demo/rank-card.png', '/demo/welcome-card.png',
    '/forms.css', '/collections.css', '/favicon.svg', '/privacy', '/terms'];
  return {
    name: 'site',
    steps: paths.map((p) => get(p)),
    server: ({ log, clock }) => createDashboardServer({ config, deps: { discord: f.discord, settings: f.settings, now: () => clock.t }, logger: log }),
  };
}

/** test/sec-dashboard.test.js and test/review-dashboard-server.test.js: the login callback limits. */
function loginScenarios() {
  const { config } = loadDashboardConfig(baseEnv());
  const make = () => {
    const exchanges = [];
    const discord = {
      async exchangeCode(code) {
        exchanges.push(code);
        if (code === 'legit_code') return { accessToken: 'token', expiresIn: 3600 };
        throw new Error('token exchange returned HTTP 400');
      },
      async getCurrentUser() { return { id: '955050000000000001', username: 'admin', globalName: null, avatar: null }; },
    };
    return {
      fakes: { act() {}, calls: () => [...exchanges] },
      server: ({ log, clock }) => createDashboardServer({ config, deps: { discord, settings: {}, now: () => clock.t }, logger: log }),
    };
  };
  const cb = (address, code = 'junk', state = 'abc', cookieState = state) => get(`/auth/callback?code=${code}&state=${state}`,
    { cookie: `${auth.STATE_COOKIE}=${cookieState}`, 'x-forwarded-for': address });
  const scenarios = [];
  const add = (name, steps) => scenarios.push({ name, steps, ...make() });
  {
    const s = [];
    for (let i = 0; i < 10; i++) s.push(cb('198.51.100.1', 'junk-code'));
    s.push(cb('198.51.100.1', 'junk-code'), cb('198.51.100.2', 'junk-code'), { advance: 60_001 }, cb('198.51.100.1', 'junk-code'), { advance: 60_001 });
    for (let i = 0; i < 13; i++) s.push(cb(`10.9.9.${i}, 198.51.100.3`, 'junk-code'));
    s.push({ advance: 60_001 });
    for (let i = 0; i < 30; i++) s.push(cb('198.51.100.4', 'junk-code', 'abc', 'different'), cb('198.51.100.4', 'no%20good'));
    s.push(cb('198.51.100.4', 'junk-code'), { advance: 60_001 });
    for (let i = 0; i <= 60; i++) s.push(cb(`203.0.113.${i}`, 'junk-code'));
    for (let i = 0; i < 60; i++) s.push(cb(`203.0.113.${i}`, 'junk-code'));
    s.push(cb('203.0.113.60', 'junk-code'), { advance: 60_001 }, cb('203.0.113.200', 'junk-code'));
    add('login-review', s);
  }
  {
    const s = [];
    for (let k = 10; k < 16; k++) for (let i = 0; i < 10; i++) s.push(cb(`198.51.100.${k}`));
    s.push(cb('203.0.113.77', 'legit_code'));
    for (let k = 20; k < 30; k++) for (let i = 0; i < 10; i++) s.push(cb(`198.51.100.${k}`));
    s.push(cb('203.0.113.78', 'legit_code'));
    add('login-junk-sources', s);
  }
  {
    const s = [];
    for (let i = 0; i < 30; i++) s.push(cb(`2001:db8:1:2::${(i + 10).toString(16)}`));
    s.push(cb('2001:DB8:1:2:abcd::1'), cb('2001:db8:9999::1', 'legit_code'), cb('2001:db8:1:3::1'), { advance: 60_001 }, cb('2001:db8:1:2::99'));
    add('login-ipv6', s);
  }
  {
    const s = [];
    for (let i = 0; i <= 60; i++) s.push(cb(`203.0.113.${i}`));
    for (let i = 0; i < 60; i++) s.push(cb(`203.0.113.${i}`));
    s.push(cb('203.0.113.60'), { advance: 60_001 }, cb('203.0.113.60'));
    add('login-first-free', s);
  }
  {
    const s = [];
    for (let i = 0; i < 60; i++) s.push(cb(`203.0.113.${i}`));
    for (let i = 0; i < 60; i++) s.push(cb(`203.0.113.${i}`));
    s.push(cb('198.51.100.9'), cb('198.51.100.9'), { advance: 60_001 }, cb('198.51.100.10', 'legit_code'), cb('198.51.100.10'), cb('198.51.100.10'));
    add('login-resolved', s);
  }
  {
    const s = [];
    const fresh = (i) => `2001:db8:${(i + 1).toString(16)}::1`;
    for (let i = 0; i < 600; i++) s.push(cb(fresh(i)));
    s.push(cb(fresh(600)), cb(fresh(0)), { advance: 60_001 }, cb(fresh(600), 'legit_code'));
    add('login-ceiling', s);
  }
  {
    const s = [];
    for (let i = 0; i < 12; i++) {
      s.push(cb(`198.51.100.50:${40000 + i}`), cb(`[2001:db8:7:7::${i + 1}]:${50000 + i}`), cb(`2001:db8:8:8::${i + 1}%eth0`));
    }
    s.push(cb('[::ffff:198.51.100.50]:6000', 'legit_code'), cb('x'.repeat(65)), cb(''));
    add('login-ports', s);
  }
  return scenarios;
}

function parkedScenario() {
  return {
    name: 'parked',
    steps: [get('/healthz'), get('/'), get('/api/me'), req('HEAD', '/healthz'), get('/healthz?x=1'), req('POST', '/api/x')],
    server: () => createParkedServer({ missing: ['DISCORD_CLIENT_SECRET', 'DASHBOARD_SESSION_SECRET'], invalid: [{ key: 'DASHBOARD_URL', reason: 'x' }] }),
    parked: { missing: ['DISCORD_CLIENT_SECRET', 'DASHBOARD_SESSION_SECRET'], invalid: ['DASHBOARD_URL'] },
  };
}

// ── Discord client ───────────────────────────────────────────────────────────────────────────────────────────────

const c = (n) => String(310000000000000000n + BigInt(n));
const R = Object.freeze({ bot: '410000000000000001', mod: '410000000000000002', fan: '410000000000000003', top: '410000000000000004', tie: '410000000000000005' });
const guildBody = (extra = {}) => ({
  id: G.zeta, name: 'Zeta', icon: 'a'.repeat(32), owner_id: GUILD_OWNER, approximate_member_count: 42, premium_tier: 2,
  roles: [
    { id: G.zeta, name: '@everyone', position: 0, color: 0, managed: false, permissions: '1024' },
    { id: R.bot, name: 'Bot', position: 5, color: 0, managed: true, permissions: String(PermissionFlagsBits.ManageRoles) },
    { id: R.mod, name: 'Mods', position: 4, color: 0xff0000, managed: false, permissions: String(PermissionFlagsBits.KickMembers) },
    { id: R.tie, name: 'Tie', position: 2, color: 0, managed: false, permissions: '0' },
    { id: R.fan, name: 'Fans', position: 2, color: 0x00ff00, managed: false, mentionable: true, permissions: '0' },
    { id: R.top, name: 'Owner', position: 9, color: 0, managed: false, permissions: '8' },
    { id: '410000000000000010', name: 'Hex', position: 1, permissions: '0x8' },
    { id: '410000000000000011', name: 'Spaces', position: 1.5, permissions: ' 4 ' },
    { id: '410000000000000012', name: 'Float', position: 1, permissions: 1.5, color: 16777216 },
    { id: '410000000000000013', name: 'Empty', permissions: '', color: 255 },
    { id: '410000000000000014', name: 'Junk', position: -1, permissions: 'abc', mentionable: 'yes' },
    { id: '410000000000000015', name: 7, position: 3, permissions: 8192 },
    { id: 'bad', name: 'bad' },
  ],
  emojis: [{ id: '610000000000000002', name: 'zebra' }, { id: '610000000000000001', name: 'apple', animated: true }, { id: null, name: '✨' },
    { id: '610000000000000003', name: 'Apple' }, { id: '610000000000000004', name: 'item10' }, { id: '610000000000000005', name: 'item9' },
    { id: '610000000000000006', name: 'éclair' }, { id: '610000000000000007', name: '_under' }, { id: '610000000000000008', name: 3 }],
  ...extra,
});
const channelsBody = [
  { id: c(1), type: 2, name: 'voice-top', position: 0 },
  { id: c(2), type: 0, name: 'text-top', position: 5 },
  { id: c(3), type: 4, name: 'CAT B', position: 2 },
  { id: c(4), type: 4, name: 'CAT A', position: 1 },
  { id: c(5), type: 0, name: 'b-text', parent_id: c(3), position: 1 },
  { id: c(6), type: 2, name: 'b-voice', parent_id: c(3), position: 0 },
  { id: c(7), type: 5, name: 'a-news', parent_id: c(4), position: 3, nsfw: true },
  { id: c(8), type: 15, name: 'a-forum', parent_id: c(4), position: 1 },
  { id: c(9), type: 0, name: 'orphan', parent_id: '399999999999999999', position: 9 },
  { id: c(10), type: 13, name: 'stage', position: 5 },
  { id: c(11), type: 99, position: 'x' },
  { id: c(12), type: 1.5, name: 'float-type' },
  { id: 'bad', type: 0 },
];
const page1 = Array.from({ length: 200 }, (_, i) => ({ id: String(300000000000000000n + BigInt(i)), name: i % 2 ? `g${i}` : '' }));

/** The REST answers of the stub: [method, path with query, status, body, headers?]. First match wins; `once` entries are used up. */
const ROUTES = [
  ['GET', `/guilds/${G.zeta}?with_counts=true`, 200, guildBody()],
  ['GET', `/guilds/${G.zeta}/channels`, 200, channelsBody],
  ['GET', `/guilds/${G.zeta}/members/123456789012345678`, 200, { roles: [R.bot, 'x', '410000000000000099'] }],
  ['GET', `/guilds/${G.beta}?with_counts=true`, 200, guildBody({ id: G.beta, roles: undefined, emojis: undefined, approximate_member_count: undefined, member_count: 7, icon: 'zz', verification_level: 2, premium_subscription_count: 3.5 })],
  ['GET', `/guilds/${G.beta}/channels`, 200, []],
  ['GET', `/guilds/${G.beta}/members/123456789012345678`, 200, { roles: [R.top] }],
  ['GET', `/guilds/${G.beta}/roles`, 200, [{ id: G.beta, name: '@everyone', position: 0, permissions: '1' }, { id: R.top, position: 3, permissions: '268435456' }]],
  ['GET', `/guilds/${G.beta}/emojis`, 200, [{ id: '610000000000000009', name: 'z' }]],
  ['GET', `/guilds/${G.plain}?with_counts=true`, 200, {}],
  ['GET', `/guilds/${G.plain}/channels`, 403, { message: 'Missing Access', code: 50001 }],
  ['GET', `/guilds/${G.plain}/members/123456789012345678`, 200, {}],
  ['GET', `/guilds/${G.zeta}/members/${USER_ID}`, 200, { roles: [RL.low, 'x', 5], user: { id: USER_ID, username: 'alice' } }],
  ['GET', `/guilds/${G.zeta}/members/${OWNER_ID}`, 404, { code: 10007 }],
  ['GET', `/guilds/${G.zeta}/members/search?query=al+ice&limit=25`, 200, [{ user: { id: USER_ID, username: 'alice', global_name: 'Alice', avatar: 'a'.repeat(32), bot: false }, nick: 'Al' }, { user: { id: 'x' } }, { nick: 'no user' }]],
  ['GET', `/guilds/${G.zeta}/members/search?query=%26%3D%C3%A9&limit=25`, 200, []],
  ['GET', `/guilds/${G.zeta}/members/search?query=bob&limit=1`, 200, [{ user: { id: OWNER_ID, username: 7, bot: true } }]],
  ['GET', `/users/${USER_ID}`, 200, { id: USER_ID, username: 'alice', global_name: null, avatar: 'a_' + 'b'.repeat(32) }],
  ['GET', `/users/${OWNER_ID}`, 404, { code: 10013 }],
  ['GET', `/users/${GUILD_OWNER}`, 200, { id: GUILD_OWNER }],
  ['GET', '/users/@me', 200, { id: USER_ID, username: 'alice', global_name: 'Alice', avatar: null }],
  ['GET', '/users/@me/guilds?limit=200', 200, [{ id: G.zeta, name: 'Z', icon: null, owner: false, permissions: '32' }, { id: 'bad' }, { id: G.alpha, owner: 'yes', permissions: 8 }, null]],
  ['GET', '/users/@me/guilds?limit=200&after=0', 200, page1],
  ['GET', `/users/@me/guilds?limit=200&after=${page1[199].id}`, 200, [{ id: G.zeta, name: 'zeta', icon: 'c'.repeat(32) }, { id: G.zeta, name: 'again' }, { id: G.beta }]],
  ['GET', `/guilds/${G.alpha}`, 403, { code: 50001 }],
  ['GET', `/guilds/${G.broken}`, 200, { id: G.broken }],
  ['GET', `/guilds/${G.stranger}`, 404, { code: 10004 }],
  ['POST', `/channels/${c(2)}/messages`, 200, { id: c(50), channel_id: c(2) }],
  ['PATCH', `/channels/${c(2)}/messages/${c(50)}`, 200, { id: c(50) }],
  ['DELETE', `/channels/${c(2)}/messages/${c(50)}`, 204],
  ['DELETE', `/channels/${c(2)}/messages/${c(51)}`, 404, { message: 'Unknown Message', code: 10008 }],
  ['POST', `/channels/${c(3)}/messages`, 403, { message: 'Missing Permissions', code: 50013 }],
  ['POST', `/channels/${c(4)}/messages`, 429, { retry_after: 1.2 }],
  ['POST', `/channels/${c(5)}/messages`, 429, {}, { 'retry-after': '7' }],
  ['POST', `/channels/${c(6)}/messages`, 429, 'not json', { 'retry-after': 'soon' }],
  ['POST', `/channels/${c(7)}/messages`, 200, 'not json'],
  ['POST', `/channels/${c(8)}/messages`, 200, { id: 'nope' }],
  ['POST', `/channels/${c(9)}/messages`, 500, { code: 1.5 }],
  ['POST', `/channels/${c(10)}/messages`, 429, { retry_after: 99999 }],
  ['POST', `/channels/${CH.text}/invites`, 200, { code: 'aB-9', expires_at: '2026-10-13T12:30:00.000Z' }],
  ['POST', `/channels/${CH.voice}/invites`, 200, { code: '<bad>' }],
  ['POST', `/channels/${CH.news}/invites`, 200, { code: 'abc', expires_at: 5 }],
];

const OPS = [
  ['getGuildResources', G.zeta], ['getGuildResources', G.beta], ['getGuildResources', G.plain], ['getGuildResources', 'nope'],
  ['getMember', G.zeta, USER_ID], ['getMember', G.zeta, OWNER_ID], ['getMember', 'x', USER_ID],
  ['searchMembers', G.zeta, ' al ice '], ['searchMembers', G.zeta, '&=é'], ['searchMembers', G.zeta, '   '], ['searchMembers', G.zeta, USER_ID],
  ['searchMembers', G.zeta, OWNER_ID], ['searchMembers', G.zeta, 'bob', 0], ['searchMembers', 'x', 'bob'],
  ['getUsers', [USER_ID, OWNER_ID, GUILD_OWNER, USER_ID, 'x']], ['getCurrentUser', TOKEN], ['getUserGuilds', USER_ID, TOKEN],
  ['getBotGuilds'], ['isBotInGuild', G.zeta, true], ['isBotInGuild', G.alpha, false], ['isBotInGuild', G.alpha, true],
  ['isBotInGuild', G.broken, true], ['isBotInGuild', G.broken, false], ['isBotInGuild', G.stranger, true], ['isBotInGuild', 'nope', true],
  ['sendMessage', c(2), { content: 'hi @everyone', allowed_mentions: { parse: ['everyone', 'users', 'roles'] } }],
  ['sendMessage', c(2), { embeds: [{ title: 'x' }] }],
  ['sendMessage', c(2), { content: '<@&1>', allowed_mentions: { roles: [RL.low, 'x'], users: [USER_ID], replied_user: true } }],
  ['sendMessage', c(2), { allowed_mentions: { parse: ['users'], users: [USER_ID] }, content: 'x' }],
  ['editMessage', c(2), c(50), { content: 'edited' }], ['deleteMessage', c(2), c(50)], ['deleteMessage', c(2), c(51)],
  ['sendMessage', c(3), { content: 'x' }], ['sendMessage', c(4), { content: 'x' }], ['sendMessage', c(5), { content: 'x' }],
  ['sendMessage', c(6), { content: 'x' }], ['sendMessage', c(7), { content: 'x' }], ['sendMessage', c(8), { content: 'x' }],
  ['sendMessage', c(9), { content: 'x' }], ['sendMessage', c(10), { content: 'x' }], ['editMessage', c(3), c(50), { content: 'x' }],
  ['sendMessage', 'nope', { content: 'x' }], ['deleteMessage', c(2), '../x'],
  ['createInvite', CH.text, 'Requested by the bot owner from the dashboard (ünï & co)'], ['createInvite', CH.voice, ''], ['createInvite', CH.news, ''],
];

function errJson(err) {
  if (!(err instanceof DiscordApiError)) return { thrown: String(err?.message ?? err) };
  return { error: { message: err.message, status: err.status, code: err.code, retryAfter: err.retryAfter ?? null, discordCode: err.discordCode ?? null } };
}

async function discordGolden() {
  const config = { clientId: '123456789012345678', clientSecret: 's', botToken: 'bot-token', redirectUri: 'http://localhost:8080/auth/callback' };
  const calls = [];
  const fetchImpl = async (url, init = {}) => {
    const u = new URL(String(url));
    const path = u.pathname.replace(/^\/api\/v10/, '') + u.search;
    const method = init.method ?? 'GET';
    calls.push({ method, path, auth: init.headers?.Authorization, contentType: init.headers?.['Content-Type'] ?? null,
      reason: init.headers?.['X-Audit-Log-Reason'] ?? null, body: init.body ? JSON.parse(init.body) : null });
    const route = ROUTES.find(([m, p]) => m === method && p === path);
    if (!route) return new Response(JSON.stringify({ code: 0 }), { status: 404 });
    const [, , status, body, headers = {}] = route;
    return new Response(body === undefined ? null : typeof body === 'string' ? body : JSON.stringify(body), { status, headers });
  };
  const api = discordApi.createDiscordApi({ config, fetchImpl, now: () => 0, sleep: async () => {} });
  const results = [];
  for (const [op, ...args] of OPS) {
    const before = calls.length;
    let result;
    try {
      let v = op === 'isBotInGuild' ? await api.isBotInGuild(args[0], { verify: args[1] })
        : op === 'searchMembers' && args.length === 3 ? await api.searchMembers(args[0], args[1], args[2])
          : op === 'createInvite' ? await api.createInvite(args[0], { reason: args[1] })
            : await api[op](...args);
      if (v instanceof Map) v = Object.fromEntries([...v].sort(([a], [b]) => (a < b ? -1 : 1)));
      result = { value: v === undefined ? null : JSON.parse(JSON.stringify(v)) };
    } catch (err) {
      result = errJson(err);
    }
    results.push({ op, args, result, calls: calls.slice(before) });
  }
  const urls = {
    invite: [api.inviteUrl(), api.inviteUrl(G.zeta), api.inviteUrl('x')],
    avatars: [[USER_ID, null], ['80351110224678912', null], [USER_ID, 'a'.repeat(32)], [USER_ID, 'a_' + 'f'.repeat(32)], [USER_ID, '../../x'],
      ['x', 'a'.repeat(32)], [null, null], ['99999999999999999999', null]].map(([id, avatar]) => ({ id, avatar, url: discordApi.userAvatarUrl({ id, avatar }) })),
    icons: [[G.zeta, null], [G.zeta, 'a'.repeat(32)], ['x', 'a'.repeat(32)], [G.zeta, 'A'.repeat(32)]].map(([id, icon]) => ({ id, icon, url: discordApi.guildIconUrl({ id, icon }) })),
    invitePermissions: discordApi.INVITE_PERMISSIONS.toString(),
    dangerousPermissions: discordApi.DANGEROUS_PERMISSIONS.toString(),
  };
  const ch = (id, type, position, parentId = null) => ({ id: c(id), type, kind: schema.channelKind(type), position, parentId });
  const sortCases = [
    [ch(1, 2, 0), ch(2, 4, 0), ch(3, 13, 0, c(2)), ch(4, 0, 9, c(2)), ch(5, 0, 1), ch(6, 0, 1)],
    [ch(9, 4, 1), ch(8, 4, 1), ch(7, 2, 0, c(9)), ch(6, 0, 0, c(8)), ch(5, 15, 0, c(8)), ch(4, 13, -1)],
  ].map((list) => ({ input: list, output: discordApi.sortChannels(list).map((x) => x.id) }));
  return { routes: ROUTES.map(([method, path, status, body, headers]) => ({ method, path, status, body: body === undefined ? null : body, raw: typeof body === 'string', headers: headers ?? {} })), results, urls, sortCases };
}

// ── Pages and collator ───────────────────────────────────────────────────────────────────────────────────────────

function pagesGolden() {
  const read = (f) => readFileSync(join(ROOT, 'src/dashboard/public', f), 'utf8');
  const changelog = readFileSync(join(ROOT, 'CHANGELOG.md'), 'utf8');
  const optionSets = [
    { name: 'none', options: {} },
    { name: 'full', options: { inviteUrl: ' https://discord.com/oauth2/authorize?client_id=1&scope=bot ', contact: { kind: 'email', value: 'a@b.co' }, supportUrl: 'https://discord.gg/x?a=1&b=2', changelog } },
    { name: 'url-contact', options: { inviteUrl: '', contact: { kind: 'url', value: 'https://x.co/"<>' }, supportUrl: 'http://not-https', changelog: '# T\n\nIntro *x*\n\n## 2026-01-02 · One\n- a\n' } },
  ];
  const out = [];
  for (const { name, options } of optionSets) {
    out.push({ page: 'landing', name, options, html: landing.renderLanding(read('landing.html'), options) });
    out.push({ page: 'docs', name, options, html: landing.renderDocs(read('docs.html'), options) });
    out.push({ page: 'changelog', name, options, html: landing.renderChangelog(read('changelog.html'), options) });
    for (const f of ['privacy.html', 'terms.html']) out.push({ page: f, name, options, html: landing.renderLegal(read(f), options) });
  }
  const errors = [];
  for (const t of ['<!--nope-->', '<!--code:nope-->', '<!--flavour:x-->', '<!--code-->', '<!--module-table:x-->', '<!--docs-toc-->']) {
    try { landing.renderLanding(t, {}); errors.push({ template: t, error: null }); } catch (err) { errors.push({ template: t, error: err.message }); }
  }
  const ok = ['<!--code:leveling--><!--flavour:leveling--><!--module-count--><!--command-count-->@@INVITE_URL@@<!--contact--><!--support--><!-- spaced -->'];
  for (const t of ok) out.push({ page: 'landing', name: 'markers', options: {}, template: t, html: landing.renderLanding(t, {}) });
  return { pages: out, errors };
}

function collatorGolden() {
  const coll = new Intl.Collator('en', { sensitivity: 'base', numeric: true });
  const names = ['zeta', 'Zeta', 'alpha', 'Alpha', 'ALPHA', 'beta', 'Broken', 'Plain', 'Bot only', 'Unknown server', 'éclair', 'eclair', 'Éclair', 'ecl',
    'item10', 'item9', 'item09', 'item1', 'item 2', 'item-2', 'item_2', 'a b', 'ab', 'a-b', 'a_b', 'a.b', 'a1', 'a10', 'a2', '10', '9', '009', '1.5',
    'server #1', 'server #2', 'server (old)', '[test]', '!bang', '~tilde', '$money', '😀 fun', '🔥 hot', 'zz top', 'Ünder', 'under', 'Ωmega', 'omega',
    'Ж', 'ж', 'Я', 'abc', 'abd', 'Ab', '', ' ', 'x', 'X', 'x2', 'x10', 'é', 'e', 'f', 'Æsir', 'aesir', 'straße', 'strasse', 'naïve', 'naive'];
  const pairs = [];
  for (const a of names) for (const b of names) pairs.push(Math.sign(coll.compare(a, b)));
  return { names, pairs };
}

// ── Write ────────────────────────────────────────────────────────────────────────────────────────────────────────

mkdirSync(OUT, { recursive: true });
writePublicTest();
const scenarios = [mainScenario(), recordsScenario(), ...registryScenarios(), rolesScenario(), httpsScenario(), joinsScenario(), siteScenario(),
  ...loginScenarios(), parkedScenario()];
const http = [];
for (const s of scenarios) {
  const result = await runScenario(s);
  if (s.parked) result.parked = s.parked;
  http.push(result);
}
writeFileSync(join(OUT, 'http.json'), `${JSON.stringify({ now: NOW, secret: SECRET, scenarios: http })}\n`);
writeFileSync(join(OUT, 'discord-api.json'), `${JSON.stringify(await discordGolden(), null, 1)}\n`);
writeFileSync(join(OUT, 'pages.json'), `${JSON.stringify(pagesGolden())}\n`);
writeFileSync(join(OUT, 'collator.json'), `${JSON.stringify(collatorGolden())}\n`);
Object.assign(console, realConsole);
console.log(`dash-server goldens: ${http.length} HTTP scenarios, ${http.reduce((n, s) => n + s.responses.length, 0)} answers`);
process.exit(0);
