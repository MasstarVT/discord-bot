// Regression tests for the dashboard's server side found by the code review. Offline: fake Discord client, no Postgres or Redis.
//   - /auth/callback is rate limited per client address and overall once a request would reach Discord's token endpoint,
//     so a flood of junk callbacks cannot get the app (or the host's address) rate limited by Discord.
//   - The automations collection refuses a message that pings a role the dashboard user could not ping (the /automation rule).
import { test, describe, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { request } from 'node:http';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Importing the automations store opens the Redis client and the Prisma client (neither is used): point them at a closed
// port, whatever the developer's environment says, unless a throw-away database was given for the opt-in cases.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const auth = await import('../src/dashboard/auth.js');
const { loadDashboardConfig } = await import('../src/dashboard/config.js');
const { createDashboardServer, clientAddress, LOGIN_RATE_LIMIT, LOGIN_GLOBAL_RATE_LIMIT } = await import('../src/dashboard/server.js');
const { validateRecordInput } = await import('../src/dashboard/collections/index.js');
const A = await import('../src/dashboard/collections/automations.js');
const { store, checkAgainstServer } = await import('../src/dashboard/collections/automations.store.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// The logger prints through console, and the Redis client reports its failed reconnects: keep the test output clean.
const realConsole = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, realConsole);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const { config } = loadDashboardConfig({
  CLIENT_ID: '123456789012345678',
  DISCORD_TOKEN: 'bot-token',
  DISCORD_CLIENT_SECRET: 'client-secret',
  DASHBOARD_SESSION_SECRET: 'x'.repeat(48),
  DATABASE_URL: 'postgresql://u:p@localhost/db',
  REDIS_URL: 'redis://localhost:6379',
});

/** One HTTP request without keep-alive. Resolves { status, headers, json }. */
function call(port, path, headers = {}) {
  return new Promise((resolvePromise, reject) => {
    const req = request({ host: '127.0.0.1', port, method: 'GET', path, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        let json;
        try { json = JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { json = undefined; }
        resolvePromise({ status: res.statusCode, headers: res.headers, json });
      });
    });
    req.on('error', reject);
    req.end();
  });
}
const listen = (server) => new Promise((res) => server.listen(0, '127.0.0.1', () => res(server.address().port)));
const close = (server) => new Promise((res) => { server.close(() => res()); server.closeAllConnections(); });
const quiet = { info() {}, warn() {}, error() {}, debug() {}, success() {} };

describe('clientAddress', () => {
  const req = (remoteAddress, forwarded) => ({ socket: { remoteAddress }, headers: forwarded === undefined ? {} : { 'x-forwarded-for': forwarded } });

  test('counts a request under its socket address, ignoring X-Forwarded-For from any other peer', () => {
    assert.equal(clientAddress(req('203.0.113.9')), '203.0.113.9');
    assert.equal(clientAddress(req('203.0.113.9', '198.51.100.1')), '203.0.113.9', 'a client cannot choose its own key');
    assert.equal(clientAddress(req('10.0.0.5', '198.51.100.1')), '10.0.0.5', 'only the loopback peer is trusted as a proxy');
  });

  test('behind a reverse proxy on the same host, the address the proxy appended is the key', () => {
    for (const peer of ['127.0.0.1', '::1', '::ffff:127.0.0.1']) {
      assert.equal(clientAddress(req(peer, '198.51.100.1')), '198.51.100.1', peer);
      assert.equal(clientAddress(req(peer, 'spoofed, 198.51.100.2')), '198.51.100.2', 'the first entries come from the client');
    }
    assert.equal(clientAddress(req('127.0.0.1')), '127.0.0.1', 'no header: the proxy itself');
    assert.equal(clientAddress(req('127.0.0.1', 'x'.repeat(200))), '127.0.0.1', 'an absurd header is not used as a key');
    assert.equal(clientAddress({ socket: {}, headers: {} }), 'unknown');
  });
});

describe('GET /auth/callback rate limit', () => {
  let server;
  let port;
  let clock;
  let exchanges;
  const advance = (ms) => { clock += ms; };

  before(async () => {
    clock = 1_800_000_000_000;
    exchanges = [];
    const discord = {
      // Every callback in these tests carries a junk code: the exchange costs a request to Discord, and fails.
      async exchangeCode(code) {
        exchanges.push(code);
        throw new Error('token exchange returned HTTP 400');
      },
    };
    server = createDashboardServer({ config, deps: { discord, settings: {}, now: () => clock }, logger: quiet });
    port = await listen(server);
  });
  after(() => close(server));

  const callback = (address, { code = 'junk-code', state = 'abc', cookieState = state } = {}) => call(
    port,
    `/auth/callback?code=${code}&state=${state}`,
    { cookie: `${auth.STATE_COOKIE}=${cookieState}`, 'x-forwarded-for': address },
  );

  test('the limits are documented numbers, generous for real logins', () => {
    assert.deepEqual({ ...LOGIN_RATE_LIMIT }, { max: 10, windowMs: 60_000 });
    assert.deepEqual({ ...LOGIN_GLOBAL_RATE_LIMIT }, { max: 60, windowMs: 60_000 });
  });

  test('one address gets 10 token exchanges a minute, then 429 without calling Discord', async () => {
    const before = exchanges.length;
    for (let i = 0; i < LOGIN_RATE_LIMIT.max; i += 1) {
      const r = await callback('198.51.100.1');
      assert.equal(r.status, 302, `attempt ${i + 1}`);
      assert.equal(r.headers.location, '/?error=login_failed');
    }
    assert.equal(exchanges.length - before, LOGIN_RATE_LIMIT.max);

    const limited = await callback('198.51.100.1');
    assert.equal(limited.status, 429);
    assert.equal(limited.json.error, 'rate_limited');
    assert.match(limited.json.message, /Too many login attempts/);
    assert.ok(limited.json.retryAfter >= 1 && limited.json.retryAfter <= 60);
    assert.equal(limited.headers['retry-after'], String(limited.json.retryAfter));
    assert.equal(exchanges.length - before, LOGIN_RATE_LIMIT.max, 'the refused request never reached Discord');

    // Another address is not affected, and the first one comes back once its minute is up.
    assert.equal((await callback('198.51.100.2')).status, 302);
    advance(LOGIN_RATE_LIMIT.windowMs + 1);
    assert.equal((await callback('198.51.100.1')).status, 302);
  });

  test('a client cannot dodge the limit by sending its own X-Forwarded-For entries', async () => {
    advance(LOGIN_RATE_LIMIT.windowMs + 1);
    const before = exchanges.length;
    let limited = 0;
    for (let i = 0; i < LOGIN_RATE_LIMIT.max + 3; i += 1) {
      const r = await callback(`10.9.9.${i}, 198.51.100.3`);
      if (r.status === 429) limited += 1;
    }
    assert.equal(limited, 3);
    assert.equal(exchanges.length - before, LOGIN_RATE_LIMIT.max);
  });

  test('requests that stop at the state or code check never reach Discord and do not count', async () => {
    advance(LOGIN_RATE_LIMIT.windowMs + 1);
    const before = exchanges.length;
    for (let i = 0; i < LOGIN_RATE_LIMIT.max * 3; i += 1) {
      const wrongState = await callback('198.51.100.4', { state: 'abc', cookieState: 'different' });
      assert.equal(wrongState.status, 302);
      const badCode = await callback('198.51.100.4', { code: 'no%20good' });
      assert.equal(badCode.status, 302);
    }
    assert.equal(exchanges.length, before);
    assert.equal((await callback('198.51.100.4')).status, 302, 'a real login after the noise still goes through');
    assert.equal(exchanges.length, before + 1);
  });

  test('all addresses together get 60 token exchanges a minute', async () => {
    advance(LOGIN_RATE_LIMIT.windowMs + 1);
    const before = exchanges.length;
    for (let i = 0; i < LOGIN_GLOBAL_RATE_LIMIT.max; i += 1) {
      assert.equal((await callback(`203.0.113.${i}`)).status, 302, `address ${i + 1}`);
    }
    assert.equal(exchanges.length - before, LOGIN_GLOBAL_RATE_LIMIT.max);
    const limited = await callback('203.0.113.200');
    assert.equal(limited.status, 429);
    assert.equal(limited.json.error, 'rate_limited');
    assert.equal(exchanges.length - before, LOGIN_GLOBAL_RATE_LIMIT.max, 'no Discord call past the ceiling');
    advance(LOGIN_GLOBAL_RATE_LIMIT.windowMs + 1);
    assert.equal((await callback('203.0.113.200')).status, 302);
  });
});

// ── Role pings in automation messages ────────────────────────────────────────────────────────────────────────────

describe('automations: a message may only ping roles the dashboard user could ping', () => {
  const GUILD = '955030000000000001';
  const USER = '955030000000000010'; // owns the fake server
  const OTHER = '955030000000000011';
  const CH = '955030000000000020';
  const LOCKED = '955030000000000030'; // not mentionable
  const LOCKED2 = '955030000000000031';
  const OPEN = '955030000000000040'; // mentionable

  function fakeDiscord() {
    const calls = [];
    const role = (id, name, mentionable, extra = {}) => ({
      id, name, position: 1, managed: false, everyone: false, assignable: true, dangerous: false, mentionable, ...extra,
    });
    const resources = {
      guild: { id: GUILD, name: 'Test', icon: null, memberCount: 3, ownerId: USER },
      channels: [{ id: CH, name: 'general', type: 0, kind: 'text', parentId: null, position: 0, nsfw: false }],
      roles: [
        role(LOCKED, 'Locked', false), role(LOCKED2, 'Locked too', false), role(OPEN, 'Pingable', true),
        role(GUILD, '@everyone', false, { everyone: true, assignable: false }),
      ],
      emojis: [],
      bot: { userId: '955030000000000099', topRolePosition: 10, manageRoles: true },
    };
    return {
      calls,
      async getGuildResources(guildId, opts) {
        calls.push(['resources', guildId, opts ?? null]);
        return resources;
      },
    };
  }

  const action = (type, extra = {}) => ({ ...A.blankAction(type), ...extra });
  const as = (member, userId = OTHER, discord = fakeDiscord()) => ({ guildId: GUILD, userId, user: null, member, discord, logger: null });
  const plain = () => as({ owner: false, permissions: '0' });
  const none = { conditions: [], actions: [] };
  const check = (ctx, actions, before = none) => checkAgainstServer(GUILD, ctx, { conditions: [], actions, before });
  const message = (type, content) => action(type, { content, channelId: type === 'SEND_MESSAGE' ? CH : null });
  const ping = (id) => `Hello <@&${id}>`;
  const refused = async (promise, path) => {
    await assert.rejects(promise, (err) => {
      assert.equal(err.name, 'RecordError', err.stack);
      assert.equal(err.code, 'invalid', err.message);
      assert.equal(err.path, path, err.message);
      assert.match(err.message, /mention|ping|exist/i);
      return true;
    });
  };

  test('every message type is checked, at the field that holds the ping', async () => {
    await refused(check(plain(), [message('SEND_MESSAGE', ping(LOCKED))]), 'actions.0.content');
    await refused(check(plain(), [action('PIN'), message('REPLY', ping(LOCKED))]), 'actions.1.content');
    await refused(check(plain(), [message('SEND_IN_THREAD', ping(LOCKED))]), 'actions.0.content');
    await assert.rejects(check(plain(), [message('REPLY', ping(LOCKED))]), /not mentionable/);
  });

  test('a mentionable role, user mentions and @everyone/@here text are fine; missing roles and the @everyone role never', async () => {
    await check(plain(), [message('SEND_MESSAGE', ping(OPEN)), message('REPLY', 'hi <@955030000000000010> @everyone @here')]);
    await refused(check(plain(), [message('REPLY', ping('955030000000000099'))]), 'actions.0.content');
    await refused(check(as({ owner: true, permissions: '8' }), [message('REPLY', ping(GUILD))]), 'actions.0.content');
  });

  test('the server owner, Administrator, Mention @everyone and the bot owner (passed as owner) may ping any role', async () => {
    const MENTION_EVERYONE = String(1n << 17n);
    for (const member of [{ owner: true, permissions: '0' }, { owner: false, permissions: '8' }, { owner: false, permissions: MENTION_EVERYONE }]) {
      await check(as(member), [message('SEND_MESSAGE', ping(LOCKED))]);
    }
    await check(as({ owner: false, permissions: '0' }, USER), [message('SEND_MESSAGE', ping(LOCKED))]); // the guild's ownerId
    // No idea who the user is: refused rather than guessed.
    await refused(check(as(undefined), [message('SEND_MESSAGE', ping(LOCKED))]), 'actions.0.content');
  });

  test('a role the stored automation already pinged stays; a new one is checked', async () => {
    const stored = { conditions: [], actions: [{ type: 'SEND_MESSAGE', content: ping(LOCKED), channelId: CH }] };
    await check(plain(), [message('SEND_MESSAGE', `${ping(LOCKED)} again`)], stored);
    await refused(check(plain(), [message('SEND_MESSAGE', ping(LOCKED2))], stored), 'actions.0.content');
  });

  test('nothing pinged: Discord is not asked at all', async () => {
    const ctx = plain();
    await check(ctx, [message('SEND_MESSAGE', 'no pings here'), action('PIN')]);
    assert.deepEqual(ctx.discord.calls, []);
  });

  test('the Welcomer preset text follows the same rule, before anything is stored', async () => {
    const create = (presetMessage, ctx) => {
      const { data } = validateRecordInput('automations', { name: 'welcome', preset: 'welcomer', presetChannelId: CH, presetMessage }, { mode: 'create' });
      return store.create(GUILD, data, ctx);
    };
    await refused(create(`Welcome ${ping(LOCKED)}`, plain()), 'presetMessage');
  });
});
