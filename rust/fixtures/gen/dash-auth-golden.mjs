// Goldens for rust/crates/dash-auth, produced by the JS dashboard code itself.
// Run from the repository root: node rust/fixtures/gen/dash-auth-golden.mjs
// Writes rust/fixtures/golden/dash-auth/*.json.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { isIP } from 'node:net';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const OUT = join(ROOT, 'rust/fixtures/golden/dash-auth');
mkdirSync(OUT, { recursive: true });
const write = (name, data) => writeFileSync(join(OUT, name), `${JSON.stringify(data, null, 2)}\n`);

const config = await import('../../../src/dashboard/config.js');
const auth = await import('../../../src/dashboard/auth.js');
const site = await import('../../../src/dashboard/siteParts.js');
const docs = await import('../../../src/dashboard/docs.js');
const changelog = await import('../../../src/dashboard/changelog.js');
const commandList = await import('../../../src/dashboard/commandList.js');
const { buildCatalog } = await import('../../../src/dashboard/catalog.js');
const server = await import('../../../src/dashboard/server.js');

// ── config ───────────────────────────────────────────────────────────────────
const base = {
  CLIENT_ID: '123456789012345678', DISCORD_TOKEN: 'bot-token', DISCORD_CLIENT_SECRET: 'client-secret',
  DASHBOARD_SESSION_SECRET: 'x'.repeat(48), DATABASE_URL: 'postgresql://localhost/db', REDIS_URL: 'redis://localhost:6379',
};
const extras = [
  {}, { DASHBOARD_URL: '   ' }, { DASHBOARD_URL: 'https://dash.example.com' }, { DASHBOARD_URL: 'http://localhost:8080' },
  { DASHBOARD_URL: 'https://dash.example.com/', DASHBOARD_PORT: '9000', BOT_OWNER_ID: '999999999999999999' },
  { DASHBOARD_URL: 'https://dash.example.com///' }, { DASHBOARD_URL: 'HTTPS://Dash.Example.COM:443' },
  { DASHBOARD_URL: 'http://dash.example.com:8443' }, { DASHBOARD_URL: 'https://dash.example.com/?' },
  { DASHBOARD_URL: 'https://dash.example.com/#' }, { DASHBOARD_URL: 'https://dash.example.com/?a=1' },
  { DASHBOARD_URL: 'https://dash.example.com/#x' }, { DASHBOARD_URL: 'https://bücher.example' },
  { DASHBOARD_URL: 'http://192.168.1.5:8080' }, { DASHBOARD_URL: 'http://[::1]:8080' },
  { DASHBOARD_SESSION_SECRET: 'short' }, { DASHBOARD_SESSION_SECRET: 'é'.repeat(31) }, { DASHBOARD_SESSION_SECRET: '😀'.repeat(16) },
  { DASHBOARD_URL: 'ftp://example.com' }, { DASHBOARD_URL: 'not a url' }, { DASHBOARD_URL: 'https://example.com/dashboard' },
  { DASHBOARD_URL: 'https://user:pw@example.com' }, { DASHBOARD_URL: 'https://user@example.com' }, { DASHBOARD_URL: 'https://' },
  { DASHBOARD_PORT: '70000' }, { DASHBOARD_PORT: 'abc' }, { DASHBOARD_PORT: '0' }, { DASHBOARD_PORT: '00080' },
  { DASHBOARD_PORT: '65535' }, { DASHBOARD_PORT: '65536' }, { DASHBOARD_PORT: '123456' }, { DASHBOARD_PORT: ' 81 ' },
  { DASHBOARD_PORT: '8e3' }, { DASHBOARD_PORT: '٣' },
  { CLIENT_ID: 'my-app' }, { CLIENT_ID: '1234567890123456' }, { CLIENT_ID: '١٢٣٤٥٦٧٨٩٠١٢٣٤٥٦٧٨' },
  { DASHBOARD_HOST: '127.0.0.1' }, { DASHBOARD_HOST: '  ' },
  { BOT_OWNER_ID: 'me' }, { BOT_OWNER_ID: ' 999999999999999999 ' },
  { DASHBOARD_TRUSTED_PROXIES: '10.0.1.30, cloudflared' }, { DASHBOARD_TRUSTED_PROXIES: '10.0.1.0/24' },
  { DASHBOARD_TRUSTED_PROXIES: ' 10.0.1.30,10.0.1.31 , ::FFFF:10.0.1.30,,fd7a::1 ' },
  { DASHBOARD_TRUSTED_PROXIES: 'fe80::1%eth0,FE80::1%ETH0, ::ffff:7f00:1' }, { DASHBOARD_TRUSTED_PROXIES: ',,' },
  { DASHBOARD_CONTACT: 'owner@example.com' }, { DASHBOARD_CONTACT: 'https://example.com/contact' },
  { DASHBOARD_CONTACT: 'https://Example.com' }, { DASHBOARD_CONTACT: 'https://example.com/a b?q=ü#f' },
  { DASHBOARD_CONTACT: 'http://example.com' }, { DASHBOARD_CONTACT: 'javascript:alert(1)' },
  { DASHBOARD_CONTACT: 'owner at example' }, { DASHBOARD_CONTACT: '<b>@x.y' }, { DASHBOARD_CONTACT: 'https://u:p@example.com' },
  { DASHBOARD_CONTACT: `${'a'.repeat(250)}@x.co` }, { DASHBOARD_CONTACT: 'a@b' },
  { DASHBOARD_SUPPORT_URL: 'https://discord.gg/abc123' }, { DASHBOARD_SUPPORT_URL: 'http://discord.gg/abc123' },
  { DASHBOARD_SUPPORT_URL: 'owner@example.com' }, { DASHBOARD_SUPPORT_URL: 'javascript:alert(1)' },
  { DASHBOARD_SUPPORT_URL: 'https://u:p@discord.gg/x' }, { DASHBOARD_SUPPORT_URL: 'https://discord.gg' },
  { DASHBOARD_URL: 'https://a.example', DASHBOARD_PORT: 'x', CLIENT_ID: 'y', DASHBOARD_SESSION_SECRET: 'z', DASHBOARD_TRUSTED_PROXIES: 'q' },
  { DATABASE_URL: '' }, { REDIS_URL: '   ' },
];
const configCases = [{ env: {}, result: config.loadDashboardConfig({}) }];
for (const extra of extras) {
  const env = { ...base, ...extra };
  configCases.push({ env, result: config.loadDashboardConfig(env) });
}
const urlInputs = ['https://dash.example.com', 'https://dash.example.com/', 'http://x:80', 'https://x:443/', 'https://x:444',
  'https://x/?', 'https://x/#', 'https://x/p', 'mailto:a@b', 'file:///c:/x', 'https://u@x', 'https://:p@x', 'http://[::ffff:1.2.3.4]',
  'https://xn--bcher-kva.example', 'https://BÜCHER.example', 'http://0x7f.1', 'http://127.1', ' https://x', 'https://x\t'];
const contactInputs = ['owner@example.com', 'https://example.com/contact', 'https://example.com', 'https://EXAMPLE.com/A',
  'https://example.com/a b', 'https://ü.example/ü', 'http://example.com', 'a.b-c+d%e@x-y.z', 'a@b', 'a@b.c.', 'https://x/?#',
  'https://x/..', 'https://x/./a/../b', 'HTTPS://X', 'https://u:@x', 'https://:@x'];
write('config.json', {
  cases: configCases,
  required: config.REQUIRED_KEYS,
  parseDashboardUrl: urlInputs.map((input) => ({ input, output: config.parseDashboardUrl(input) })),
  parseContact: contactInputs.map((input) => ({ input, output: config.parseContact(input) })),
});

// ── IP helpers ───────────────────────────────────────────────────────────────
const ipInputs = ['1.2.3.4', '01.2.3.4', '255.255.255.255', '256.1.1.1', '1.2.3', '1.2.3.4.5', '::', '::1', '1::', 'fe80::1%eth0',
  'fe80::1%', 'fe80::1%e.t-h:0', 'FE80::1', '::ffff:1.2.3.4', '::FFFF:1.2.3.4', '::ffff:01.2.3.4', '1:2:3:4:5:6:7:8', '1:2:3:4:5:6:7::',
  '::2:3:4:5:6:7:8', '1:2:3:4:5:6:7:8:9', '1::2::3', '12345::', 'g::1', '1:2:3:4:5:6:1.2.3.4', '::1.2.3.4', '1::1.2.3.4',
  '2001:db8::', ' 1.2.3.4', '1.2.3.4 ', '', 'localhost', '::ffff:127.0.0.1', '0.0.0.0', '1:2:3:4:5::1.2.3.4', '1:2:3:4:5:6::1.2.3.4',
  '[::1]', '::%eth0', 'fe80::1%eth0%x', '1.2.3.4%x'];
const ipCases = ipInputs.map((input) => ({ input, isIP: isIP(input), normalizeIp: config.normalizeIp(input) }));
const keyInputs = ['198.51.100.7:51234', '[2001:db8:1:2::a]:443', '2001:db8:1:2::b', '[2001:db8:1:2::a]', '[fe80::1%eth0]:80',
  'fe80::9', '[::ffff:203.0.113.9]:5000', '::ffff:203.0.113.9', '::FFFF:203.0.113.9%eth0', '2001:DB8:1:2:0:0:0:f',
  '2001:db8:1:2:ffff:ffff:ffff:ffff', '2001:db8:1:3::a', '::1', 'fe80::1%eth0', '::ffff:cb00:7109', '203.0.113.9', 'unknown',
  'not an ip', '', ' 203.0.113.9 ', '1:2:3:4:5:6:7::', '::2:3:4:5:6:7:8', '1:2:3:4:5:6:1.2.3.4', '::1.2.3.4', '[1.2.3.4]:80',
  '1.2.3.4:99999', '01.2.3.4:80', '::ffff:0:1.2.3.4', '::ffff:1.2.3.4:80', '64:ff9b::1.2.3.4', '0:0:0:0:0:ffff:1.2.3.4'];
const loopback = ['127.0.0.1', '::1', '::ffff:127.0.0.1'];
const trusted = new Set(['10.0.1.30', 'fd7a::1']);
const clientCases = [];
for (const peer of ['203.0.113.9', '10.0.0.5', ...loopback, '10.0.1.30', '::ffff:10.0.1.30', 'FD7A::1', undefined]) {
  for (const fwd of [undefined, '198.51.100.1', 'spoofed, 198.51.100.2', 'x'.repeat(200), ' , ', '1.1.1.1,', 'é'.repeat(64), '😀'.repeat(33)]) {
    const req = { socket: { remoteAddress: peer }, headers: fwd === undefined ? {} : { 'x-forwarded-for': fwd } };
    clientCases.push({ peer: peer ?? null, forwarded: fwd ?? null, trusted: false, output: server.clientAddress(req) });
    clientCases.push({ peer: peer ?? null, forwarded: fwd ?? null, trusted: true, output: server.clientAddress(req, trusted) });
  }
}
write('ip.json', {
  ipCases,
  loginSourceKey: keyInputs.map((input) => ({ input, output: server.loginSourceKey(input) })),
  clientAddress: { trusted: [...trusted], cases: clientCases },
  limits: {
    WRITE_RATE_LIMIT: server.WRITE_RATE_LIMIT, SEARCH_RATE_LIMIT: server.SEARCH_RATE_LIMIT, LOGIN_RATE_LIMIT: server.LOGIN_RATE_LIMIT,
    LOGIN_GLOBAL_RATE_LIMIT: server.LOGIN_GLOBAL_RATE_LIMIT, LOGIN_FIRST_ATTEMPT_RATE_LIMIT: server.LOGIN_FIRST_ATTEMPT_RATE_LIMIT,
  },
});

// ── rate limiter sequence ────────────────────────────────────────────────────
{
  let t = 1_000_000;
  const hit = server.createRateLimiter({ max: 3, windowMs: 10_000, now: () => t });
  const steps = [];
  for (const [dt, key] of [[0, 'a'], [100, 'a'], [100, 'b'], [100, 'a'], [100, 'a'], [2_500, 'a'], [6_000, 'a'], [1, 'a'],
    [1_200, 'a'], [0, 'b'], [20_000, 'a'], [0, 'a'], [0, 'a'], [0, 'a'], [999, 'a'], [8_901, 'a'], [100, 'a']]) {
    t += dt;
    steps.push({ at: t, key, result: hit(key) });
  }
  write('rate-limiter.json', { max: 3, windowMs: 10_000, steps });
}

// ── auth ─────────────────────────────────────────────────────────────────────
const secrets = ['x'.repeat(48), 'y'.repeat(48), 'a', 'pässwörd-😀-secret-with-unicode-32+', ' spaced secret '];
const keys = secrets.map((secret) => ({ secret, keyHex: auth.deriveSessionKey(secret).toString('hex') }));
const FAR = 4_102_444_800_000; // 2100-01-01
const payloads = [
  { uid: '111111111111111111', username: 'alice', globalName: 'Alice', avatar: null, accessToken: 'user-access-token-abc123', exp: FAR },
  { uid: '222222222222222222', username: 'bób 😀 "q" \\ </script>', globalName: null, avatar: 'a_0123456789abcdef', accessToken: 't'.repeat(512), exp: FAR, extra: [1, 2.5, { n: true }] },
  { exp: FAR + 0.5 },
];
const sealed = [];
for (const { secret } of keys.slice(0, 2)) {
  const key = auth.deriveSessionKey(secret);
  for (const payload of payloads) sealed.push({ secret, payload, token: auth.sealSession(payload, key) });
}
// Values that must not open (built by the JS, checked against the JS).
const key0 = auth.deriveSessionKey(secrets[0]);
const good = auth.sealSession(payloads[0], key0);
const body = good.slice(3);
const mid = Math.floor(body.length / 2);
const badTokens = [
  `v1.${body.slice(0, mid)}${body[mid] === 'A' ? 'B' : 'A'}${body.slice(mid + 1)}`, good.slice(0, -4), `v2.${body}`,
  auth.sealSession({ ...payloads[0], exp: 'never' }, key0), auth.sealSession([1, 2], key0), auth.sealSession(null, key0),
  auth.sealSession({ exp: 1000 }, key0), auth.sealSession(42, key0), '', 'v1.', 'v1.!!!', 'abc', `v1.${'A'.repeat(5000)}`,
  `v1.${body}=`, ` ${good}`, `V1.${body}`, `${good}A`, `${good}AA`,
];
const unsealCases = badTokens.map((token) => ({ token, now: Date.now(), output: auth.unsealSession(token, key0) }));
unsealCases.push({ token: good, now: FAR, output: auth.unsealSession(good, key0, FAR) });
unsealCases.push({ token: good, now: FAR - 1, output: auth.unsealSession(good, key0, FAR - 1) });

const cookieHeaders = ['a=1; dash_session=v1.abc; a=2; bad=%E0%A4%A; q="quoted"; =x; flag', '', 'x=%C3%A9; y=a+b; z=%2B%20',
  ' sp = v ; t="a"b"; u="; v=""', 'w=%ED%A0%80; v=%e2%82%ac; n=%zz; m=%', 'dash_oauth_state=abc_-123; a="%41"', 'é=ü; ü=%C3%BC',
  'a=b=c; ;;  ; k=  ', 'x="'];
const cookieCases = cookieHeaders.map((header) => ({ header, entries: [...auth.parseCookies(header)] }));
const serializeCases = [];
for (const [name, value, opts] of [
  ['dash_session', 'v1.x', { maxAge: 60.9, secure: true }], ['dash_session', 'v1.x', {}], ['a', 'é ;=,"', { path: '/auth' }],
  ['a', '', { maxAge: 0 }], ['a', 'b', { maxAge: -5 }], ['a', 'b', { maxAge: 0.5 }], ['a', "~!*'()", { maxAge: 604800, path: '/', secure: false }],
  ['a', 'b', { maxAge: 1e21 }], ['a', 'b', { maxAge: 123456789.99 }],
]) serializeCases.push({ name, value, opts, output: auth.serializeCookie(name, value, opts) });
const clearCases = [['dash_session', {}], ['dash_oauth_state', { path: '/auth', secure: true }]]
  .map(([name, opts]) => ({ name, opts, output: auth.clearCookie(name, opts) }));

const authorizeCases = [
  ['123456789012345678', 'https://d.example/auth/callback', 'st'],
  ['123456789012345678', 'http://localhost:8080/auth/callback', 'abc_-DEF123'],
  ["a b'~*!()é", 'https://x.example/a b?c=d&e=f#g', 'x y+z/=&%'],
].map(([clientId, redirectUri, state]) => ({ clientId, redirectUri, state, output: auth.buildAuthorizeUrl({ clientId, redirectUri }, state) }));

// The token calls, through a fake fetch: the request the JS sends and how it reads each answer.
const creds = [
  { clientId: '123456789012345678', clientSecret: 'client-secret', redirectUri: 'https://d.example/auth/callback' },
  { clientId: "a b'~*é:", clientSecret: 'p@ss:wörd+/=', redirectUri: 'http://localhost:8080/auth/callback' },
];
const capture = async (fn) => {
  let req = null;
  const fetchImpl = async (url, init) => { req = { url, method: init.method, headers: init.headers, body: init.body, redirect: init.redirect }; return new Response('{}', { status: 400 }); };
  await fn(fetchImpl).catch(() => {});
  return req;
};
const requests = [];
for (const config of creds) {
  requests.push({ kind: 'exchange', config, code: 'abc.DEF~1-2_3', request: await capture((fetchImpl) => auth.exchangeCode({ config, fetchImpl }, 'abc.DEF~1-2_3')) });
  requests.push({ kind: 'revoke', config, token: 'tok en+/=', request: await capture((fetchImpl) => auth.revokeToken({ config, fetchImpl }, 'tok en+/=')) });
}
const ok = (o) => JSON.stringify({ access_token: 'tok', token_type: 'Bearer', scope: 'identify guilds', expires_in: 604800, ...o });
const responses = [
  [200, ok({})], [400, ok({})], [401, ''], [500, 'oops'], [302, ''], [200, 'not json'], [200, 'null'], [200, '"str"'], [200, '[]'],
  [200, ok({ access_token: '' })], [200, ok({ access_token: 5 })], [200, ok({ access_token: 'a'.repeat(513) })],
  [200, ok({ access_token: 'a'.repeat(512) })], [200, ok({ access_token: '😀'.repeat(256) })], [200, ok({ access_token: '😀'.repeat(257) })],
  [200, ok({ token_type: 'bearer' })], [200, ok({ token_type: 'BEARER' })], [200, ok({ token_type: 'mac' })], [200, ok({ token_type: undefined })],
  [200, ok({ token_type: ['bearer'] })], [200, ok({ token_type: null })], [200, ok({ scope: 'guilds identify email' })],
  [200, ok({ scope: 'identify' })], [200, ok({ scope: 'identify  guilds' })], [200, ok({ scope: ['identify', 'guilds'] })],
  [200, ok({ expires_in: 3600 })], [200, ok({ expires_in: '3600' })], [200, ok({ expires_in: ' 0x10 ' })], [200, ok({ expires_in: 0 })],
  [200, ok({ expires_in: -5 })], [200, ok({ expires_in: null })], [200, ok({ expires_in: 'abc' })], [200, ok({ expires_in: 1.5 })],
  [200, ok({ expires_in: [7] })], [200, ok({ expires_in: true })], [200, ok({ expires_in: '' })], [200, ok({ expires_in: '1e3' })],
  [200, ok({ expires_in: 'Infinity' })], [200, ok({ expires_in: '.5' })], [200, ok({ expires_in: '0b11' })], [204, ''],
];
const responseCases = [];
for (const [status, body] of responses) {
  const fetchImpl = async () => new Response(status === 204 ? null : body, { status });
  try {
    const r = await auth.exchangeCode({ config: creds[0], fetchImpl }, 'code');
    responseCases.push({ status, body, ok: r });
  } catch (err) {
    responseCases.push({ status, body, err: { message: err.message, status: err.status ?? null } });
  }
}
const revokeCases = [];
for (const status of [200, 204, 400, 503]) {
  revokeCases.push({ status, output: await auth.revokeToken({ config: creds[0], fetchImpl: async () => new Response(null, { status }) }, 't') });
}
revokeCases.push({ status: null, output: await auth.revokeToken({ config: creds[0], fetchImpl: async () => { throw new Error('down'); } }, 't') });

const guildCases = [
  { owner: true, permissions: '0' }, { owner: false, permissions: '8' }, { owner: false, permissions: '32' },
  { owner: false, permissions: '1099511627775' }, { owner: false, permissions: '0' }, { owner: false, permissions: '16' },
  { owner: false, permissions: '1099511627735' }, { owner: false, permissions: 'abc' }, { owner: false, permissions: '-8' },
  { owner: false }, { owner: 'true', permissions: '0' }, null, [], 'x', 8, { permissions: 8 }, { permissions: '9'.repeat(30) },
  { permissions: '9'.repeat(31) }, { permissions: '000000000000000000000000000040' }, { permissions: ' 8' }, { permissions: '٨' },
  { owner: 1, permissions: '0' }, { permissions: '340282366920938463463374607431768211455' },
].map((guild) => ({ guild, output: auth.canManageGuild(guild) }));
guildCases.push({ guild: '__undefined__', output: auth.canManageGuild(undefined) });

const json = { 'content-type': 'application/json', 'x-dashboard': '1' };
const writeReqs = [
  ['GET', json], ['PUT', json], ['DELETE', { 'x-dashboard': '1' }], ['DELETE', json], ['POST', { 'x-dashboard': '1' }],
  ['POST', { 'content-type': 'text/plain', 'x-dashboard': '1' }], ['POST', { 'content-type': 'application/x-www-form-urlencoded', 'x-dashboard': '1' }],
  ['POST', { 'content-type': 'application/json' }], ['POST', { ...json, 'x-dashboard': 'true' }], ['PATCH', { ...json, origin: 'https://evil.example' }],
  ['PATCH', { ...json, origin: 'null' }], ['PATCH', { ...json, origin: 'https://dash.example.com.evil.example' }],
  ['PATCH', { ...json, 'sec-fetch-site': 'cross-site' }], ['PATCH', { ...json, 'sec-fetch-site': 'same-site' }], ['PATCH', json],
  ['POST', { ...json, 'content-type': 'Application/JSON; charset=utf-8' }], ['POST', { ...json, 'content-type': '  application/jsonx' }],
  ['PATCH', { ...json, origin: 'https://dash.example.com', 'sec-fetch-site': 'same-origin' }], ['POST', { ...json, 'sec-fetch-site': 'none' }],
  ['patch', json], ['PATCH', { ...json, 'x-dashboard': ' 1' }],
  ...['http://192.168.1.5:8080', 'http://localhost:8080', 'https://evil.example', 'https://dash.example.com.evil.example', 'null', '',
    'not an origin', 'ftp://dash.example.com', 'https://evil.example/path', 'javascript:alert(1)', 'https://DASH.example.com',
    'https://dash.example.com/', 'https://dash.example.com:443', 'http://[::1]:8080', 'https://bücher.example', 'https://xn--bcher-kva.example']
    .map((origin) => ['PATCH', { ...json, origin }]),
  ['PATCH', { ...json, origin: 'https://dash.example.com', 'sec-fetch-site': 'cross-site' }],
];
const writeCases = writeReqs.map(([method, headers]) => ({ method, headers, output: auth.checkWriteRequest({ method, headers }, { origin: 'https://dash.example.com' }) }));

write('auth.json', {
  constants: {
    SESSION_COOKIE: auth.SESSION_COOKIE, STATE_COOKIE: auth.STATE_COOKIE, SESSION_MAX_AGE_SECONDS: auth.SESSION_MAX_AGE_SECONDS,
    STATE_MAX_AGE_SECONDS: auth.STATE_MAX_AGE_SECONDS, OAUTH_SCOPES: auth.OAUTH_SCOPES, OAUTH_TIMEOUT_MS: auth.OAUTH_TIMEOUT_MS,
  },
  keys, sealed, unsealCases, cookieCases, serializeCases, clearCases, authorizeCases, requests, responseCases, revokeCases,
  guildCases, writeCases,
});

// ── site parts, docs, changelog, command list ───────────────────────────────
const texts = ['', 'plain', '<a href="x">&\'</a>', 'Set up achievements (MEE6 Achievements, /achievements-reset)',
  'Ban a member (MEE6 parity).', 'Text (MEE6) more', 'Ends (MEE6 x)  ', 'No trailing (MEE6', '  spaced  ',
  'XP per message. MEE6 uses 15-25. Changes apply at once.', 'One.Two.  Three MEE6s. Four MEE6.', 'MEE6\u00a0first. Then.',
  'Mr. Smith uses MEE6.\tSecond.\nThird.', 'é😀 <x>', 'a.   b', 'Ends with dot.'];
const md = ['plain', '`code` and **bold** and [link](https://x.example/a?b=c&d)', '[bad](http://x.example) `<x>`', '**a*b**',
  '``', '`a` `b`', '[a](https://x y)', '[](https://x)', 'unclosed `code', '<script>**x**</script>', '😀`😀`**😀**[😀](https://😀.example)',
  '[a]](https://x)', '**[a](https://b)**', '`**x**`', '[x](https://a)(b)'];
const contacts = [null, { kind: 'email', value: 'owner@example.com' }, { kind: 'email', value: ' owner@example.com ' },
  { kind: 'email', value: 'a b@c' }, { kind: 'email', value: 'a@b@c' }, { kind: 'email', value: '"x"@y' }, { kind: 'url', value: 'https://x.example/?a="b"&c' },
  { kind: 'url', value: 'HTTP://x' }, { kind: 'url', value: 'javascript:alert(1)' }, { kind: 'url', value: 5 }, { kind: 'other', value: 'a@b' },
  { kind: 'email', value: 'é@ü' }];
const supports = [null, 'https://discord.gg/abc', ' HTTPS://discord.gg/x?a=<b> ', 'http://discord.gg/x', '', 5];
const fieldsForDefault = [
  { type: 'int', default: 15, label: 'XP' }, { type: 'int', default: 15, unit: 'seconds', label: 'Cooldown (seconds)' },
  { type: 'int', default: 15, unit: 'seconds', label: 'Cooldown' }, { type: 'float', default: 1.5, unit: 'x', label: 'Multiplier' },
  { type: 'float', default: 0.1 }, { type: 'int', default: 0 }, { type: 'int', default: null }, { type: 'int' }, { type: 'text', default: '' },
  { type: 'text', default: 'Hello {user}' }, { type: 'text', default: 'x'.repeat(80) }, { type: 'text', default: 'x'.repeat(81) },
  { type: 'text', default: '😀'.repeat(40) }, { type: 'text', default: '😀'.repeat(41) }, { type: 'boolean', default: true },
  { type: 'boolean', default: false }, { type: 'boolean' }, { type: 'boolean', default: 0 }, { type: 'boolean', default: 'x' },
  { type: 'enum', default: 'B', options: [{ value: 'A', label: 'Ay' }, { value: 'B', label: 'Bee' }] },
  { type: 'enum', default: 'C', options: [{ value: 'A', label: 'Ay' }] }, { type: 'enum', default: 1, options: [{ value: '1', label: 'str' }, { value: 1, label: 'num' }] },
  { type: 'enum', default: 'A' }, { type: 'channel', default: null }, { type: 'list', default: [] }, { type: 'int', default: 3, unit: 'min', label: 'Delay (MIN)' },
  { type: 'int', default: 1e21 }, { type: 'float', default: 12345678901234567890 },
];
const siteCases = {
  escapeHtml: texts.map((t) => ({ input: t, output: site.escapeHtml(t) })),
  publicDescription: texts.map((t) => ({ input: t, output: site.publicDescription(t) })),
  publicNote: texts.map((t) => ({ input: t, output: site.publicNote(t) })),
  inlineMarkdown: [...md, ...texts].map((t) => ({ input: t, output: site.inlineMarkdown(t) })),
  renderContact: contacts.map((c) => ({ input: c, output: site.renderContact(c) })),
  renderSupport: supports.map((s) => ({ input: s, output: site.renderSupport(s), labelled: site.renderSupport(s, 'Join <us>') })),
  formatDefault: fieldsForDefault.map((f) => ({ input: f, output: site.formatDefault(f) })),
  flavourOf: ['safety', 'community', 'engagement', 'tools', 'nope', ''].map((id) => ({ input: id, output: site.flavourOf(id) })),
  WHO_LABEL: site.WHO_LABEL, REPO_URL: site.REPO_URL, ISSUES_URL: site.ISSUES_URL,
};

// The docs from the real catalog and command list; the JSON round trip is what the Rust side reads.
const liveCatalog = buildCatalog();
const catalog = JSON.parse(JSON.stringify(liveCatalog));
const commands = JSON.parse(readFileSync(join(ROOT, 'src/dashboard/commands.json'), 'utf8'));
assert.equal(docs.renderDocsModules(catalog, commands), docs.renderDocsModules(liveCatalog, commands));
assert.equal(docs.renderDocsToc(catalog), docs.renderDocsToc(liveCatalog));
const scalar = Object.entries(catalog.modules).map(([key, mod]) => ({ key, settings: site.scalarSettings(mod).map((s) => ({ field: s.field.field, value: s.value })) }));
// A synthetic catalog with awkward values.
const oddCatalog = {
  categories: [
    { id: 'tools', label: 'Tools & <More>', description: 'Desc "q"', modules: ['m1', 'm2'] },
    { id: 'unknown', label: 'Other', description: '', modules: [] },
  ],
  modules: {
    m1: { code: 'M1', label: 'Mod <1>', description: "It's", default: true, configCommand: '/m1 setup', groups: [
      { fields: [{ type: 'int', default: 5, unit: 'seconds', label: 'Wait', description: 'Waits. MEE6 waits 3. Done.' },
        { type: 'boolean', default: false, label: 'Flag' }, { type: 'channel', default: null, label: 'Chan' }] }] },
    m2: { code: 'M2', label: 'Mod 2', description: 'Two', default: false, configCommand: null, groups: [] },
  },
};
const oddCommands = [
  { name: 'm1-do', description: 'Does (MEE6 /x)', module: 'm1', who: 'moderators', subcommands: [{ name: 'a b', description: 'A <b> (MEE6 c).' }] },
  { name: 'free', description: 'Core', module: null, who: 'strange', subcommands: [] },
  { name: 'm1-two', description: '', module: 'm1', who: 'owner', subcommands: [] },
];
const changelogMd = readFileSync(join(ROOT, 'CHANGELOG.md'), 'utf8');
const oddMd = '# Title\r\nIntro line one.\r\n  Intro two  \r\n- stray item\r\n## 2026-01-02 · First <entry>  \r\n- item `a` **b**\r\n-not item\r\n'
  + '## not a date · x\n##  2026-13-40   ·   Second\n- [l](https://x.example)\n\n# skip\nlater text\n## 2026-01-01 ·\n## 2025-12-31 · Third\n';
const parsedReal = changelog.parseChangelog(changelogMd);
const parsedOdd = changelog.parseChangelog(oddMd);
write('html.json', {
  site: siteCases,
  docs: {
    catalog, commands,
    toc: docs.renderDocsToc(catalog), modules: docs.renderDocsModules(catalog, commands), core: docs.renderCoreCommands(commands),
    scalar,
    odd: { catalog: oddCatalog, commands: oddCommands, toc: docs.renderDocsToc(oddCatalog), modules: docs.renderDocsModules(oddCatalog, oddCommands), core: docs.renderCoreCommands(oddCommands) },
    ids: { module: docs.moduleId('levels'), command: docs.commandId('rank') },
  },
  changelog: {
    markdown: changelogMd, parsed: parsedReal, latest: changelog.renderLatestChanges(parsedReal.entries),
    latest5: changelog.renderLatestChanges(parsedReal.entries, 5), entries: changelog.renderChangelogEntries(parsedReal.entries),
    oddMarkdown: oddMd, oddParsed: parsedOdd, oddEntries: changelog.renderChangelogEntries(parsedOdd.entries),
    oddLatest: changelog.renderLatestChanges(parsedOdd.entries), empty: changelog.renderChangelogEntries([]),
    emptyParsed: changelog.parseChangelog(''),
  },
});

// ── command list ─────────────────────────────────────────────────────────────
const golden = JSON.parse(readFileSync(join(ROOT, 'rust/fixtures/golden/commands.json'), 'utf8'));
const entries = golden.map((c) => ({ ns: { module: c.module, permissionLevel: c.permissionLevel }, payload: c.payload }));
const summary = commandList.summarizeCommands(entries);
const names = ['a', 'a-b', 'a_b', 'ab', 'a1', 'a-1', 'a_1', 'a10', 'a2', '1a', '_a', '-a', 'b', 'a-', 'a_', 'A', 'aB', 'ab-c', 'ab_c', 'abc', 'a--b', 'a__b', 'a-_b', 'a_-b', 'z9', 'z-9', 'é', 'e', 'f'];
const sorted = commandList.summarizeCommands(names.map((name) => ({ ns: {}, payload: { name, description: '' } }))).map((c) => c.name);
write('command-list.json', {
  input: golden.map((c) => ({ module: c.module, permissionLevel: c.permissionLevel ?? null, payload: c.payload })),
  summary,
  committed: commands,
  names, sorted,
});

console.log('dash-auth goldens written to', OUT);
process.exit(0);
