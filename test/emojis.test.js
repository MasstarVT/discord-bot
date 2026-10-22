// pkg-emojis tests (pkg-emojis.md §8 plus fake-interaction runs of /emoji add | list | delete). Fully offline: no Discord, Postgres or
// Redis. The image download and the create timeout go through the module's test seam; the one un-seamed run uses an IP literal that
// safeFetch refuses before any network access.
import { test, after, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { existsSync, readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL ??= 'postgresql://test:test@127.0.0.1:1/test';
// Nothing listens here; the client retries in the background until after().
process.env.REDIS_URL ??= 'redis://127.0.0.1:1';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC_FILE = resolve(ROOT, 'src/commands/admin/emoji.js');

const { Collection, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const m = await import('../src/commands/admin/emoji.js');
const { SafeFetchError } = await import('../src/utils/safeFetch.js');
const { TimeoutError } = await import('../src/utils/async.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  Object.assign(console, quiet);
  m.__setTestHooks();
  const { default: redis } = await import('../src/services/redis.js');
  redis.disconnect();
});
afterEach(() => m.__setTestHooks());

const E = (id, name, o = {}) => ({ id, name, animated: false, managed: false, available: true, roles: { cache: { size: 0 } }, ...o });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The payload size Discord checks (same as test/smoke.test.js).
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

// ── 1. Payload ────────────────────────────────────────────────────────────────

test('payload: name, subcommands, permissions, size, options and metadata exports', () => {
  const json = m.data.toJSON();
  assert.equal(json.name, 'emoji');
  assert.deepEqual(json.options.map((o) => o.name), ['add', 'list', 'delete']);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild | PermissionFlagsBits.ManageGuildExpressions));
  assert.equal(json.default_member_permissions, '1073741856');
  assert.ok(charCount(json) <= 300, `payload is ${charCount(json)} characters`);

  const add = json.options.find((o) => o.name === 'add').options;
  const emoji = add.find((o) => o.name === 'emoji');
  assert.equal(emoji.type, 3);
  assert.equal(emoji.required, true);
  assert.equal(emoji.max_length, 512);
  const name = add.find((o) => o.name === 'name');
  assert.equal(name.type, 3);
  assert.ok(!name.required);
  assert.equal(name.min_length, 2);
  assert.equal(name.max_length, 32);
  const role = add.find((o) => o.name === 'role');
  assert.equal(role.type, 8);
  assert.ok(!role.required);
  assert.deepEqual(add.map((o) => o.name), ['emoji', 'name', 'role'], 'required options come first');

  const del = json.options.find((o) => o.name === 'delete').options.find((o) => o.name === 'emoji');
  assert.equal(del.required, true);
  assert.equal(del.autocomplete, true);
  assert.equal(del.max_length, 100);
  for (const sub of json.options) assert.ok(sub.description.length <= 100);

  assert.equal(m.module, null);
  assert.equal(m.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(m.gated, undefined);
  assert.equal(m.allowInDMs, undefined);
  assert.equal(typeof m.autocomplete, 'function');
  assert.equal(typeof m.execute, 'function');
  assert.deepEqual(buildCommandPayload(m).contexts, [0]);
});

// ── 2. parseEmojiSource ───────────────────────────────────────────────────────

test('parseEmojiSource: every input shape', () => {
  const p = m.parseEmojiSource;
  assert.deepEqual(p('<:pog:123456789012345678>'), { kind: 'custom', id: '123456789012345678', name: 'pog', animated: false });
  assert.deepEqual(p(' <a:dance:123456789012345678> '), { kind: 'custom', id: '123456789012345678', name: 'dance', animated: true });
  assert.deepEqual(
    p('https://cdn.discordapp.com/emojis/123456789012345678.webp?size=48&name=pog&quality=lossless'),
    { kind: 'custom', id: '123456789012345678', name: 'pog', animated: false },
  );
  assert.deepEqual(
    p('https://cdn.discordapp.com/emojis/123456789012345678.gif'),
    { kind: 'custom', id: '123456789012345678', name: null, animated: true },
  );
  assert.equal(p('https://cdn.discordapp.com/emojis/123456789012345678.GIF').animated, true);
  assert.equal(p('https://media.discordapp.net/emojis/123456789012345678.webp?animated=true').animated, true);

  const u = p('<https://example.com/img/party-cat.png>');
  assert.equal(u.kind, 'url');
  assert.equal(u.url, 'https://example.com/img/party-cat.png');
  assert.equal(u.nameHint, 'party_cat');
  assert.equal(p('https://example.com/a b.png').kind, 'url');
  // Another host's /emojis/ path is just an image URL.
  assert.equal(p('https://example.com/emojis/123456789012345678.png').kind, 'url');

  assert.equal(p('http://').kind, 'invalid');
  assert.deepEqual(p('123456789012345678'), { kind: 'id', id: '123456789012345678' });
  for (const s of [':pog:', ':pog~1:']) assert.deepEqual(p(s), { kind: 'shortcode', name: 'pog' }, s);
  // A bare word is not a shortcode: it is not an emoji or a link at all.
  for (const s of ['pog', 'notanemoji', ':pog', 'pog:']) assert.equal(p(s).kind, 'invalid', s);
  assert.equal(p('😀').kind, 'unicode');
  assert.equal(p('🇺🇸').kind, 'unicode');
  assert.equal(p('').kind, 'empty');
  assert.equal(p('   ').kind, 'empty');
  assert.equal(p(null).kind, 'empty');
  for (const s of ['ftp://x/y.png', '<:x:12>', 'hello world!']) assert.equal(p(s).kind, 'invalid', s);
});

// ── 3. sanitizeEmojiName / resolveEmojiName ───────────────────────────────────

test('sanitizeEmojiName and resolveEmojiName', () => {
  assert.equal(m.sanitizeEmojiName('Pog Champ'), 'Pog_Champ');
  assert.equal(m.sanitizeEmojiName('café-time'), 'cafe_time');
  assert.equal(m.sanitizeEmojiName('my.emoji'), 'my_emoji');
  assert.equal(m.sanitizeEmojiName('hello!!'), 'hello');
  assert.equal(m.sanitizeEmojiName('😀😀'), '');
  assert.equal(m.sanitizeEmojiName('x'.repeat(40)).length, 32);

  assert.equal(m.resolveEmojiName({ given: 'a!' }).ok, false);
  assert.match(m.resolveEmojiName({ given: 'a!' }).reason, /2–32/);
  const spaced = m.resolveEmojiName({ given: 'Pog Champ' });
  assert.equal(spaced.ok, true);
  assert.equal(spaced.name, 'Pog_Champ');
  assert.equal(spaced.adjusted, true);
  assert.equal(m.resolveEmojiName({ given: 'pog' }).adjusted, false);
  assert.equal(m.resolveEmojiName({ given: ' pog ' }).adjusted, false, 'surrounding whitespace is not an adjustment');
  const bare = m.resolveEmojiName({ given: null, source: { kind: 'custom', name: null } });
  assert.equal(bare.name, 'emoji');
  assert.equal(bare.derived, true);
  assert.equal(m.resolveEmojiName({ given: null, source: { kind: 'url', nameHint: null } }).name, 'emoji');
  assert.equal(m.resolveEmojiName({ given: null, source: { kind: 'custom', name: 'pog' } }).name, 'pog');
  assert.equal(m.resolveEmojiName({ given: null, source: { kind: 'custom', name: '12345' } }).name, 'emoji', 'all-digit names fall back');
});

// ── 4. nameFromUrl ────────────────────────────────────────────────────────────

test('nameFromUrl', () => {
  assert.equal(m.nameFromUrl('https://x.com/a/party-cat.gif'), 'party_cat');
  assert.equal(m.nameFromUrl('https://x.com/123456.png'), null);
  assert.equal(m.nameFromUrl('https://x.com/i.png?name=cool_one'), 'cool_one');
  assert.equal(m.nameFromUrl('https://x.com/'), null);
  assert.equal(m.nameFromUrl('not a url'), null);
  assert.equal(m.nameFromUrl('https://x.com/%E0%A4%A.png'), 'E0A4A', 'a bad percent escape is kept raw, then sanitised');
  assert.equal(m.nameFromUrl('https://x.com/caf%C3%A9.png'), 'cafe', 'a valid escape is decoded first');
});

// ── 5. pickFreeName ───────────────────────────────────────────────────────────

test('pickFreeName', () => {
  assert.equal(m.pickFreeName('pog', new Set()), 'pog');
  assert.equal(m.pickFreeName('pog', new Set(['pog'])), 'pog_2');
  assert.equal(m.pickFreeName('POG', new Set(['pog', 'pog_2'])), 'POG_3');
  const long = 'x'.repeat(32);
  const cut = m.pickFreeName(long, new Set([long]));
  assert.equal(cut, `${'x'.repeat(30)}_2`);
  assert.equal(cut.length, 32);
  const all = new Set(['pog']);
  for (let n = 2; n <= 99; n++) all.add(`pog_${n}`);
  assert.equal(m.pickFreeName('pog', all), null);
  assert.deepEqual([...m.takenNames([E('1', 'Pog'), E('2', 'CAT', { managed: true })])].sort(), ['cat', 'pog']);
});

// ── 6. emojiSlotLimit ─────────────────────────────────────────────────────────

test('emojiSlotLimit', () => {
  const cases = [[[0], 50], [[1], 100], [[2], 150], [[3], 250], [[undefined], 50], [[0, ['MORE_EMOJI']], 200],
    [[2, ['MORE_EMOJI']], 200], [[3, ['MORE_EMOJI']], 250], [[0, ['VANITY_URL']], 50], [[7], 50], [[null], 50]];
  for (const [args, want] of cases) assert.equal(m.emojiSlotLimit(...args), want, JSON.stringify(args));
});

// ── 7. countEmojiSlots / formatSlots ──────────────────────────────────────────

test('countEmojiSlots and formatSlots', () => {
  const list = [
    E('1', 'a'), E('2', 'b'), E('3', 'c'),
    E('4', 'd', { animated: true }), E('5', 'e', { animated: true }),
    E('6', 'f', { managed: true }),
    E('7', 'g', { available: false }),
  ];
  const guild = { premiumTier: 1, features: [] };
  const s = m.countEmojiSlots(list, guild);
  assert.equal(s.static.used, 4);
  assert.equal(s.static.max, 100);
  assert.equal(s.static.free, 96);
  assert.equal(s.animated.used, 2);
  assert.equal(s.tier, 1);
  assert.equal(m.formatSlots(s), 'Static 4/100 · Animated 2/100 · Boost level 1');
  assert.deepEqual(m.countEmojiSlots(new Map(list.map((e) => [e.id, e])), guild), s);
  assert.deepEqual(m.countEmojiSlots(new Collection(list.map((e) => [e.id, e])), guild), s);
  // Over the limit after a boost was lost: free never goes negative.
  const over = m.countEmojiSlots(Array.from({ length: 60 }, (_, i) => E(String(i), `e${i}`)), { premiumTier: 0, features: [] });
  assert.equal(over.static.free, 0);
  assert.equal(m.countEmojiSlots([], undefined).static.max, 50);
});

// ── 8. sniffImage ─────────────────────────────────────────────────────────────

test('sniffImage', () => {
  const pad = Buffer.alloc(32);
  const png = Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), pad]);
  assert.deepEqual(m.sniffImage(png), { mime: 'image/png', ext: 'png', animated: false });
  assert.deepEqual(m.sniffImage(Buffer.concat([Buffer.from([0xff, 0xd8, 0xff, 0xe0]), pad])), { mime: 'image/jpeg', ext: 'jpg', animated: false });
  assert.deepEqual(m.sniffImage(Buffer.concat([Buffer.from('GIF89a'), pad])), { mime: 'image/gif', ext: 'gif', animated: true });
  assert.equal(m.sniffImage(Buffer.concat([Buffer.from('GIF87a'), pad])).animated, true);
  const webp = (chunk, flags) => {
    const b = Buffer.concat([Buffer.from('RIFF'), Buffer.alloc(4), Buffer.from('WEBP'), Buffer.from(chunk), Buffer.alloc(32)]);
    b[20] = flags;
    return b;
  };
  assert.deepEqual(m.sniffImage(webp('VP8 ', 0)), { mime: 'image/webp', ext: 'webp', animated: false });
  assert.deepEqual(m.sniffImage(webp('VP8X', 0x02)), { mime: 'image/webp', ext: 'webp', animated: true });
  assert.equal(m.sniffImage(webp('VP8X', 0x10)).animated, false);
  assert.equal(m.sniffImage(Buffer.from('<svg xmlns="http://www.w3.org/2000/svg"></svg>')), null);
  assert.equal(m.sniffImage(Buffer.from('<html><body>not an image at all</body></html>')), null);
  assert.equal(m.sniffImage(Buffer.alloc(4)), null);
  assert.equal(m.sniffImage('not a buffer'), null);
  assert.equal(m.sniffImage(null), null);
});

// ── 9. emojiCdnUrl ────────────────────────────────────────────────────────────

test('emojiCdnUrl', () => {
  assert.equal(m.emojiCdnUrl('1', true), 'https://cdn.discordapp.com/emojis/1.gif');
  assert.equal(m.emojiCdnUrl('1', false), 'https://cdn.discordapp.com/emojis/1.png');
  assert.equal(m.emojiCdnUrl('1', false, { size: 96 }), 'https://cdn.discordapp.com/emojis/1.png?size=96');
});

// ── 10. describeEmojiError ────────────────────────────────────────────────────

test('describeEmojiError', () => {
  const err = (code) => ({ code, message: 'm' });
  assert.match(m.describeEmojiError(err(30008)), /static/);
  assert.match(m.describeEmojiError(err(30018)), /animated/);
  assert.match(m.describeEmojiError(err(50013), { action: 'add' }), /Create Expressions/);
  assert.match(m.describeEmojiError(err(50013), { action: 'delete' }), /Manage Expressions/);
  assert.doesNotMatch(m.describeEmojiError(err(50013), { action: 'delete' }).split('(')[0], /Create Expressions/);
  assert.match(m.describeEmojiError(err(50045)), /256 KB/);
  assert.match(m.describeEmojiError(err(50138)), /resize/);
  assert.match(m.describeEmojiError(err(50046)), /read that image/);
  assert.match(m.describeEmojiError(err(10014)), /no longer exists/);
  assert.match(m.describeEmojiError(err(50001)), /access/);
  assert.ok(m.describeEmojiError({ code: 50035, message: 'bad name' }).startsWith('Discord rejected the emoji:'));
  assert.ok(m.describeEmojiError(err(99999)).startsWith('Discord returned an error:'));
  assert.ok(m.describeEmojiError(new Error('boom')).startsWith('Discord returned an error:'));
  const t = new TimeoutError('x');
  assert.match(m.describeEmojiError(t, { action: 'add' }), /queued/);
  assert.match(m.describeEmojiError(t, { action: 'add' }), /\/emoji list/);
  assert.match(m.describeEmojiError(t, { action: 'delete' }), /deletion is queued/);
  assert.match(m.describeEmojiError(err('30008')), /static/, 'a string code goes through Number()');
});

// ── 11. describeFetchError ────────────────────────────────────────────────────

test('describeFetchError', () => {
  const f = (code, msg, extra) => new SafeFetchError(code, msg, extra);
  assert.match(m.describeFetchError(f('TOO_LARGE', 'big')), /256 KB/);
  assert.match(m.describeFetchError(f('HTTP_ERROR', 404), { fromCdn: true }), /CDN/);
  assert.match(m.describeFetchError(f('HTTP_ERROR', 404)), /HTTP 404/);
  assert.match(m.describeFetchError(f('HTTP_ERROR', 500), { fromCdn: true }), /HTTP 500/);
  assert.match(m.describeFetchError(f('BLOCKED_ADDRESS', 'x')), /private/);
  assert.equal(m.describeFetchError(f('INVALID_URL', 'Only http and https URLs are allowed.')), 'Only http and https URLs are allowed.');
  assert.match(m.describeFetchError(f('DNS_FAILED', 'x')), /host name/);
  assert.match(m.describeFetchError(f('TIMEOUT', 'x')), /8 s/);
  assert.match(m.describeFetchError(f('TOO_MANY_REDIRECTS', 'x')), /redirects/);
  assert.match(m.describeFetchError(f('BAD_CONTENT_TYPE', 'x')), /PNG, JPG, GIF or WebP/);
  assert.match(m.describeFetchError(f('BAD_IMAGE', 'x')), /not a valid image/);
  assert.match(m.describeFetchError(f('NETWORK', 'x')), /network error/);
  assert.equal(m.describeFetchError(f('WEIRD', 'x')), 'The image could not be downloaded.');
  assert.equal(m.describeFetchError(new Error('x')), 'The image could not be downloaded.');
  assert.equal(m.describeFetchError(null), 'The image could not be downloaded.');
});

// ── 12. formatEmojiLine / buildListPages ──────────────────────────────────────

test('formatEmojiLine and buildListPages', () => {
  assert.equal(m.formatEmojiLine(E('1', 'pog')), '<:pog:1> `:pog:` · `1`');
  const loaded = m.formatEmojiLine(E('2', 'dance', { animated: true, managed: true, available: false, roles: { cache: { size: 2 } } }));
  assert.ok(loaded.startsWith('<a:dance:2>'));
  const at = ['animated', '🔒 2 roles', 'managed', '⚠️ unavailable'].map((t) => loaded.indexOf(t));
  assert.ok(at.every((i) => i >= 0), loaded);
  assert.deepEqual([...at].sort((a, b) => a - b), at, 'tags keep their order');
  assert.match(m.formatEmojiLine(E('3', 'x', { roles: { cache: { size: 1 } } })), /🔒 1 role$/);

  const statics = Array.from({ length: 30 }, (_, i) => E(String(100 + i), `s${String(i).padStart(2, '0')}`));
  const animated = Array.from({ length: 15 }, (_, i) => E(String(200 + i), `a${String(i).padStart(2, '0')}`, { animated: true }));
  const slots = m.countEmojiSlots([...animated, ...statics], { premiumTier: 0, features: [] });
  const pages = m.buildListPages([...animated, ...statics], slots);
  assert.equal(pages.length, 3);
  assert.deepEqual(pages.map((p) => p.description.split('\n').length - 2), [20, 20, 5]);
  const lines = pages.flatMap((p) => p.description.split('\n').slice(2));
  const firstAnimated = lines.findIndex((l) => l.startsWith('<a:'));
  assert.equal(firstAnimated, 30);
  assert.ok(lines.slice(firstAnimated).every((l) => l.startsWith('<a:')));
  for (const p of pages) {
    assert.ok(p.description.startsWith(m.formatSlots(slots)));
    assert.ok(p.description.length <= 4096);
    assert.equal(p.footer, '45 custom emojis');
  }
  assert.deepEqual(m.buildListPages([], slots), []);
  assert.equal(m.buildListPages([E('1', 'one')], slots)[0].footer, '1 custom emoji');
  assert.equal(m.buildListPages(statics, slots, { perPage: 999 }).length, 2, 'perPage is clamped to 25');
  // Ties on the name sort by id.
  const tie = m.buildListPages([E('9', 'same'), E('2', 'Same')], slots)[0].description.split('\n').slice(2);
  assert.ok(tie[0].includes('`2`') && tie[1].includes('`9`'));
});

// ── 13. resolveGuildEmoji ─────────────────────────────────────────────────────

test('resolveGuildEmoji', () => {
  const cache = [E('111111111111111111', 'pog'), E('222222222222222222', 'Pog'), E('333333333333333333', 'cat')];
  const r = (s) => m.resolveGuildEmoji(cache, s);
  assert.equal(r('<:x:111111111111111111>').emoji, cache[0]);
  assert.match(r('<:x:999999999999999999>').reason, /isn't from this server/);
  assert.equal(r('333333333333333333').ok, true);
  assert.match(r('444444444444444444').reason, /No emoji with that ID/);
  assert.equal(r('pog').emoji, cache[0], 'the exact match wins');
  assert.equal(r('Pog').emoji, cache[1]);
  assert.equal(r(':CAT:').emoji, cache[2], 'case-insensitive');
  const amb = r('POG');
  assert.equal(amb.ok, false);
  assert.match(amb.reason, /Several emojis/);
  assert.match(r('dog').reason, /No emoji named/);
  assert.match(r('').reason, /Pick an emoji/);
  assert.equal(m.resolveGuildEmoji(new Collection(cache.map((e) => [e.id, e])), 'cat').ok, true);
});

// ── 14. emojiChoices ──────────────────────────────────────────────────────────

test('emojiChoices', () => {
  const pool = Array.from({ length: 30 }, (_, i) => E(String(100000000000000000n + BigInt(i)), `e${String(i).padStart(2, '0')}`));
  pool.push(E('555555555555555555', 'zz', { managed: true }));
  pool[7] = { ...pool[7], animated: true };
  const all = m.emojiChoices(pool, '');
  assert.equal(all.length, 25);
  assert.ok(!all.some((c) => c.name.includes('zz')));
  assert.deepEqual(m.emojiChoices(pool, 'e1').map((c) => c.name), Array.from({ length: 10 }, (_, i) => `:e1${i}:`));
  assert.deepEqual(m.emojiChoices(pool, '<:e05:100000000000000005>').map((c) => c.name), [':e05:']);
  assert.ok(m.emojiChoices(pool, 'e07')[0].name.endsWith(' (animated)'));
  for (const c of all) assert.match(c.value, /^\d{17,21}$/);
  assert.deepEqual(m.emojiChoices(pool, 'zz'), [], 'managed emojis are not offered');
  // startsWith ranks before a mere substring.
  const ranked = m.emojiChoices([E('1', 'xcat'), E('2', 'cat'), E('3', 'acat')], 'cat').map((c) => c.name);
  assert.deepEqual(ranked, [':cat:', ':acat:', ':xcat:']);
});

// ── 15. Upload guard ──────────────────────────────────────────────────────────

test('upload guard', () => {
  const t1 = m.beginEmojiUpload('g1', 0);
  assert.ok(t1);
  assert.equal(m.beginEmojiUpload('g1', 1000), null);
  m.endEmojiUpload('g1', Symbol('other'));
  assert.equal(m.beginEmojiUpload('g1', 1000), null, 'a foreign token keeps the entry');
  m.endEmojiUpload('g1', t1);
  const t2 = m.beginEmojiUpload('g1', 2000);
  assert.ok(t2, 'cleared');
  m.endEmojiUpload('g1', t2);

  const t3 = m.beginEmojiUpload('g2', 0);
  assert.ok(m.beginEmojiUpload('g2', m.IN_FLIGHT_STALE_MS + 1), 'a stale entry is replaced');
  m.endEmojiUpload('g2', t3);                       // the stale upload settling late must not clear the newer entry
  assert.equal(m.beginEmojiUpload('g2', m.IN_FLIGHT_STALE_MS + 2), null);
});

// ── 16. Autocomplete wiring ───────────────────────────────────────────────────

test('autocomplete wiring', async () => {
  const cache = new Map(Array.from({ length: 40 }, (_, i) => {
    const id = String(100000000000000000n + BigInt(i));
    return [id, E(id, `po${i}`)];
  }));
  const make = ({ sub = 'delete', focused = { name: 'emoji', value: 'po' }, throws = false } = {}) => {
    const got = [];
    return {
      got,
      interaction: {
        options: {
          getSubcommand: () => sub,
          getFocused: () => { if (throws) throw new Error('boom'); return focused; },
        },
        guild: { emojis: { cache } },
        respond: async (c) => { got.push(c); },
      },
    };
  };
  const a = make();
  await m.autocomplete(a.interaction);
  assert.equal(a.got.length, 1);
  assert.ok(a.got[0].length > 0 && a.got[0].length <= 25);
  for (const c of a.got[0]) assert.match(c.value, /^\d{17,21}$/);

  const b = make({ sub: 'add' });
  await m.autocomplete(b.interaction);
  assert.deepEqual(b.got, [[]]);

  const c = make({ throws: true });
  await m.autocomplete(c.interaction);
  assert.deepEqual(c.got, [[]]);

  const d = make({ focused: { name: 'name', value: 'po' } });
  await m.autocomplete(d.interaction);
  assert.deepEqual(d.got, [[]]);
});

// ── 17/18. Static checks ──────────────────────────────────────────────────────

test('static checks on the command file', () => {
  const text = readFileSync(SRC_FILE, 'utf8');
  assert.ok(!/database\/client\.js/.test(text));
  assert.ok(!/services\/redis\.js/.test(text));
  assert.ok(!/console\./.test(text));
  assert.ok(!/setCustomId\(/.test(text));
  assert.ok(!/customId:/.test(text));
  const noLineComments = text.split('\n').map((l) => l.replace(/\/\/.*$/, '')).join('\n');
  assert.ok(!/\bemoji\.url\b/.test(noLineComments), 'the deprecated emoji.url getter');
  assert.equal(existsSync(resolve(ROOT, 'src/plugins/emojis.js')), false);

  // Release safety: the create promise owns the guard through the two-callback form, never .finally().
  const then = text.indexOf('createP.then(');
  assert.ok(then > 0);
  assert.match(text.slice(then, then + 140), /createP\.then\(\(\) => endEmojiUpload\(guild\.id, token\), \(\) => endEmojiUpload\(guild\.id, token\)\)/);
  assert.ok(text.indexOf('handedOff = true;', then) > then);
  assert.ok(!text.includes('createP.finally('));
  // Uploads are always a sniffed data URI, never a URL or a bare Buffer.
  assert.ok(text.includes('attachment: `data:${info.mime};base64,'));
  assert.ok(text.includes('maxBytes: EMOJI_MAX_BYTES'));
});

// ── Fake-interaction runs ─────────────────────────────────────────────────────

const GUILD_ID = '900000000000000001';
const PNG = Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), Buffer.alloc(64)]);
const GIF = Buffer.concat([Buffer.from('GIF89a'), Buffer.alloc(64)]);

function fakeGuild({ emojis = [], tier = 0, features = [], botPerms = ['ManageGuildExpressions'], create, remove, fetch } = {}) {
  const cache = new Collection(emojis.map((e) => [e.id, e]));
  const guild = {
    id: GUILD_ID, name: 'Test *Guild*', premiumTier: tier, features,
    members: { me: { permissions: new PermissionsBitField(botPerms.map((k) => PermissionFlagsBits[k])) } },
    emojis: { cache, calls: { create: [], delete: [] } },
  };
  guild.emojis.create = async (opts) => {
    guild.emojis.calls.create.push(opts);
    if (create) return create(opts, guild);
    const em = {
      id: '800000000000000001', name: opts.name, animated: opts.attachment.startsWith('data:image/gif'),
      toString() { return `<:${opts.name}:800000000000000001>`; },
      imageURL: (o) => `https://cdn.discordapp.com/emojis/800000000000000001.${o.extension}?size=${o.size}`,
    };
    cache.set(em.id, em);
    return em;
  };
  guild.emojis.delete = async (id, reason) => {
    guild.emojis.calls.delete.push({ id, reason });
    if (remove) return remove(id, guild);
    return undefined;
  };
  guild.emojis.fetch = fetch ?? (async () => cache);
  return guild;
}

function fakeInteraction(guild, {
  sub, strings = {}, role = null, perms = ['ManageGuildExpressions'], confirm = null,
} = {}) {
  const calls = [];
  let lastPayload = null;
  const i = {
    guild, guildId: guild.id, user: { id: '700000000000000001', tag: 'tester#0' },
    member: { permissions: new PermissionsBitField(perms.map((k) => PermissionFlagsBits[k])) },
    deferred: false, replied: false, ephemeral: null,
    options: {
      getSubcommand: () => sub,
      getString: (n) => strings[n] ?? null,
      getRole: () => role,
    },
    calls,
    reply: async (p) => {
      i.replied = true;
      lastPayload = p;
      calls.push(['reply', p]);
      // awaitConfirm's message: a fake collector answered by `confirm('yes' | 'no' | null)`.
      const collector = new EventEmitter();
      const message = {
        id: 'm1',
        createMessageComponentCollector: () => {
          setTimeout(() => {
            confirm?.beforeAnswer?.();
            const ids = lastPayload.components?.[0]?.components?.map((c) => c.data.custom_id) ?? [];
            if (confirm?.answer === 'yes' || confirm?.answer === 'no') {
              const customId = confirm.answer === 'yes' ? ids[0] : ids[1];
              collector.emit('collect', { customId, update: async () => {} });
            }
            collector.emit('end');
          }, 5);
          return collector;
        },
        edit: async () => {},
      };
      return { resource: { message } };
    },
    deferReply: async (p) => { i.deferred = true; i.ephemeral = Boolean(p?.ephemeral); calls.push(['defer', p]); },
    editReply: async (p) => { calls.push(['editReply', p]); return { id: 'm1', createMessageComponentCollector: () => new EventEmitter() }; },
    followUp: async (p) => { calls.push(['followUp', p]); return {}; },
  };
  return i;
}

const embedOf = (call) => call[1].embeds[0].toJSON();
const lastCall = (i) => i.calls[i.calls.length - 1];
const guardIsFree = () => {
  const t = m.beginEmojiUpload(GUILD_ID);
  if (t) m.endEmojiUpload(GUILD_ID, t);
  return Boolean(t);
};
const cdn = '<:pog:123456789012345678>';

test('add: permission and validation failures are ephemeral and happen before the defer', async () => {
  const guild = fakeGuild({ emojis: [E('1', 'taken'), E('123456789012345679', 'inside')] });
  const run = async (opts, guildOverride = guild) => {
    const i = fakeInteraction(guildOverride, { sub: 'add', ...opts });
    await m.execute(i);
    assert.equal(i.deferred, false, 'no defer before validation');
    assert.equal(i.calls.length, 1);
    assert.equal(i.calls[0][0], 'reply');
    assert.equal(i.calls[0][1].ephemeral, true);
    return embedOf(i.calls[0]);
  };

  assert.match((await run({ strings: { emoji: cdn }, perms: ['ManageGuild'] })).description, /You need the \*\*Manage Expressions\*\* permission/);
  const noBot = fakeGuild({ botPerms: ['SendMessages'] });
  assert.match((await run({ strings: { emoji: cdn } }, noBot)).description, /I need the \*\*Manage Expressions\*\*/);
  assert.equal((await run({ strings: { emoji: '😀' } })).title.includes('Invalid Emoji'), true);
  assert.match((await run({ strings: { emoji: '😀' } })).description, /built into Discord/);
  assert.match((await run({ strings: { emoji: ':pog:' } })).description, /Copy Link/);
  assert.match((await run({ strings: { emoji: '123456789012345678' } })).description, /ID alone/);
  assert.match((await run({ strings: { emoji: 'hello world!' } })).description, /direct image link/);
  assert.match((await run({ strings: { emoji: '<:inside:123456789012345679>' } })).description, /already in this server/);
  assert.match((await run({ strings: { emoji: cdn }, role: { id: GUILD_ID } })).description, /@everyone/);
  assert.match((await run({ strings: { emoji: cdn, name: 'a!' } })).description, /2–32/);
  assert.match((await run({ strings: { emoji: cdn, name: 'TAKEN' } })).description, /already exists/);
  assert.ok(guardIsFree());
});

test('add: no free slots and an upload already running', async () => {
  const full = fakeGuild({ emojis: Array.from({ length: 50 }, (_, i) => E(String(i + 1), `e${i}`)) });
  const i = fakeInteraction(full, { sub: 'add', strings: { emoji: cdn } });
  await m.execute(i);
  assert.equal(i.deferred, false);
  assert.match(embedOf(i.calls[0]).description, /no free static emoji slots \(50\/50, boost level 0\)/);

  // A full static side does not block an animated custom emoji.
  m.__setTestHooks({ download: async () => GIF });
  const i2 = fakeInteraction(full, { sub: 'add', strings: { emoji: '<a:dance:123456789012345678>' } });
  await m.execute(i2);
  assert.equal(i2.deferred, true);
  assert.equal(full.emojis.calls.create.length, 1);

  // A link needs only one kind to be free, and refuses when both are full.
  const bothFull = fakeGuild({ emojis: [
    ...Array.from({ length: 50 }, (_, n) => E(String(n + 1), `s${n}`)),
    ...Array.from({ length: 50 }, (_, n) => E(String(n + 100), `a${n}`, { animated: true })),
  ] });
  const i3 = fakeInteraction(bothFull, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
  await m.execute(i3);
  assert.equal(i3.deferred, false);
  assert.match(embedOf(i3.calls[0]).description, /no free emoji slots \(Static 50\/50 · Animated 50\/50 · Boost level 0\)/);

  const g = fakeGuild();
  const token = m.beginEmojiUpload(GUILD_ID);
  const busy = fakeInteraction(g, { sub: 'add', strings: { emoji: cdn } });
  await m.execute(busy);
  assert.equal(busy.deferred, false);
  assert.equal(embedOf(busy.calls[0]).title.includes('Upload In Progress'), true);
  m.endEmojiUpload(GUILD_ID, token);
});

test('add: success uploads a sniffed data URI, applies the role and reports the slots', async () => {
  let asked = null;
  m.__setTestHooks({ download: async (source) => { asked = source; return PNG; } });
  const guild = fakeGuild({ emojis: [E('1', 'pog')] });
  const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: cdn }, role: { id: '600000000000000001' } });
  await m.execute(i);

  assert.deepEqual(asked, { kind: 'custom', id: '123456789012345678', name: 'pog', animated: false });
  assert.deepEqual(i.calls.map((c) => c[0]), ['defer', 'editReply']);
  assert.equal(i.calls[0][1].ephemeral, false, 'deferred publicly');
  const [opts] = guild.emojis.calls.create;
  assert.equal(opts.attachment, `data:image/png;base64,${PNG.toString('base64')}`);
  assert.equal(opts.name, 'pog_2', 'a derived name that is taken gets a suffix');
  assert.deepEqual(opts.roles, ['600000000000000001']);
  assert.equal(opts.reason, 'tester#0 (700000000000000001) via /emoji add');

  const embed = embedOf(i.calls[1]);
  assert.match(embed.description, /`:pog_2:` is ready to use/);
  assert.match(embed.description, /<@&600000000000000001>/);
  assert.match(embed.description, /`pog` was taken, so it was named `pog_2`/);
  assert.match(embed.description, /\*\*Slots:\*\* Static 2\/50 · Animated 0\/50 · Boost level 0/);
  assert.match(embed.thumbnail.url, /\.png\?size=128$/);
  assert.deepEqual(i.calls[1][1].allowedMentions, { parse: [] });
  assert.ok(guardIsFree(), 'the guard is released after a successful create');
});

test('add: an explicit name is used as given, and adjusted names are reported', async () => {
  m.__setTestHooks({ download: async () => GIF });
  const guild = fakeGuild();
  const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'https://example.com/cat.gif', name: 'Party Cat' } });
  await m.execute(i);
  const [opts] = guild.emojis.calls.create;
  assert.equal(opts.name, 'Party_Cat');
  assert.equal(opts.roles, undefined);
  assert.ok(opts.attachment.startsWith('data:image/gif;base64,'));
  const embed = embedOf(lastCall(i));
  assert.match(embed.description, /The name was adjusted to `Party_Cat`/);
  assert.match(embed.thumbnail.url, /\.gif\?size=128$/);
  assert.match(embed.description, /Animated 1\/50/);
});

test('add: download, image-type and slot-recheck failures answer publicly and release the guard', async () => {
  const guild = fakeGuild();
  const run = async (strings = { emoji: 'https://example.com/x.png' }) => {
    const i = fakeInteraction(guild, { sub: 'add', strings });
    await m.execute(i);
    assert.equal(guild.emojis.calls.create.length, 0);
    assert.equal(i.deferred, true);
    assert.ok(guardIsFree(), 'the guard is released');
    return embedOf(lastCall(i));
  };

  m.__setTestHooks({ download: async () => { throw new SafeFetchError('TOO_LARGE', 'big'); } });
  let e = await run();
  assert.equal(e.title.includes('Download Failed'), true);
  assert.match(e.description, /256 KB/);

  m.__setTestHooks({ download: async () => { throw new SafeFetchError('HTTP_ERROR', 404); } });
  assert.match((await run({ emoji: cdn })).description, /CDN doesn't have that emoji/);

  m.__setTestHooks({ download: async () => { throw new TypeError('unexpected'); } });
  assert.match((await run()).description, /could not be downloaded/);

  m.__setTestHooks({ download: async () => Buffer.from('<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>') });
  e = await run();
  assert.equal(e.title.includes('Unsupported Image'), true);

  // The last static slot disappears while the image downloads.
  const racing = fakeGuild({ emojis: Array.from({ length: 49 }, (_, n) => E(String(n + 1), `e${n}`)) });
  m.__setTestHooks({ download: async () => { racing.emojis.cache.set('999', E('999', 'late')); return PNG; } });
  const i = fakeInteraction(racing, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
  await m.execute(i);
  assert.equal(racing.emojis.calls.create.length, 0);
  assert.match(embedOf(lastCall(i)).description, /no free static emoji slots/);
  assert.ok(guardIsFree());
});

test('add: a link to a private address is refused by safeFetch (real download path, no network)', async () => {
  const guild = fakeGuild();
  const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'http://127.0.0.1/x.png' } });
  await m.execute(i);
  assert.equal(guild.emojis.calls.create.length, 0);
  assert.match(embedOf(lastCall(i)).description, /private or reserved network address/);
  assert.ok(guardIsFree());

  const cloud = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'http://169.254.169.254/latest/meta-data' } });
  await m.execute(cloud);
  assert.match(embedOf(lastCall(cloud)).description, /private or reserved network address/);
});

test('add: Discord errors are mapped, and the guard is released when the create promise settles', async () => {
  m.__setTestHooks({ download: async () => PNG });
  const guild = fakeGuild({ create: async () => { throw Object.assign(new Error('Maximum number of emojis reached'), { code: 30008 }); } });
  const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
  await m.execute(i);
  const e = embedOf(lastCall(i));
  assert.equal(e.title.includes('Upload Failed'), true);
  assert.match(e.description, /no free static emoji slots/);
  await sleep(0);
  assert.ok(guardIsFree());

  const denied = fakeGuild({ create: async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); } });
  const i2 = fakeInteraction(denied, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
  await m.execute(i2);
  assert.match(embedOf(lastCall(i2)).description, /Create Expressions/);
  await sleep(0);
  assert.ok(guardIsFree());
});

test('add: a create that outlasts the timeout reports "queued" and keeps the guard until it settles', async () => {
  m.__setTestHooks({ download: async () => PNG, createTimeout: 15 });
  let finish;
  const guild = fakeGuild({
    create: (opts) => new Promise((resolve) => {
      finish = () => resolve({ id: '800000000000000002', name: opts.name, animated: false });
    }),
  });
  const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
  await m.execute(i);
  const e = embedOf(lastCall(i));
  assert.equal(e.title.includes('Upload Queued'), true);
  assert.match(e.description, /queued/);
  assert.equal(guardIsFree(), false, 'the queued create still owns the guard');
  const second = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'https://example.com/y.png' } });
  await m.execute(second);
  assert.equal(second.deferred, false);
  assert.equal(embedOf(second.calls[0]).title.includes('Upload In Progress'), true);

  finish();
  await sleep(0);
  assert.ok(guardIsFree(), 'released once the create settles');
});

test('add: a create that fails after the timeout does not become an unhandled rejection', async () => {
  m.__setTestHooks({ download: async () => PNG, createTimeout: 10 });
  let fail;
  const guild = fakeGuild({ create: () => new Promise((_, reject) => { fail = () => reject(Object.assign(new Error('late'), { code: 50013 })); }) });
  const unhandled = [];
  const onUnhandled = (err) => unhandled.push(err);
  process.on('unhandledRejection', onUnhandled);
  try {
    const i = fakeInteraction(guild, { sub: 'add', strings: { emoji: 'https://example.com/x.png' } });
    await m.execute(i);
    fail();
    await sleep(20);
    assert.deepEqual(unhandled, []);
    assert.ok(guardIsFree());
  } finally {
    process.off('unhandledRejection', onUnhandled);
  }
});

test('list: empty server, and a single page', async () => {
  const empty = fakeGuild();
  const i = fakeInteraction(empty, { sub: 'list', perms: [] });
  await m.execute(i);
  assert.deepEqual(i.calls.map((c) => c[0]), ['defer', 'editReply']);
  assert.equal(i.calls[0][1].ephemeral, false, 'public');
  assert.match(embedOf(i.calls[1]).description, /no custom emojis yet/);
  assert.match(embedOf(i.calls[1]).description, /Static 0\/50/);

  const guild = fakeGuild({ emojis: [E('1', 'pog'), E('2', 'dance', { animated: true }), E('3', 'twitch', { managed: true })], tier: 2 });
  const j = fakeInteraction(guild, { sub: 'list' });
  await m.execute(j);
  const e = embedOf(lastCall(j));
  assert.match(e.title, /Emojis in Test \*Guild\*/);
  assert.match(e.description, /Static 1\/150 · Animated 1\/150 · Boost level 2/);
  assert.match(e.description, /<:pog:1>/);
  assert.match(e.description, /<a:dance:2>.*animated/);
  assert.match(e.description, /managed/);
  assert.equal(e.footer.text, '3 custom emojis');

  // A failing fetch falls back to the cache.
  const broken = fakeGuild({ emojis: [E('1', 'pog')], fetch: async () => { throw new Error('nope'); } });
  const k = fakeInteraction(broken, { sub: 'list' });
  await m.execute(k);
  assert.match(embedOf(lastCall(k)).description, /<:pog:1>/);
});

test('delete: permission, lookup and managed failures are ephemeral and never reach the confirm', async () => {
  const guild = fakeGuild({ emojis: [E('111111111111111111', 'pog'), E('222222222222222222', 'Pog'), E('333333333333333333', 'twitch', { managed: true })] });
  const run = async (opts, g = guild) => {
    const i = fakeInteraction(g, { sub: 'delete', ...opts });
    await m.execute(i);
    assert.equal(i.calls.length, 1);
    assert.equal(i.calls[0][1].ephemeral, true);
    assert.equal(i.calls[0][1].components, undefined, 'no confirm dialog');
    return embedOf(i.calls[0]);
  };
  assert.match((await run({ strings: { emoji: 'pog' }, perms: ['ManageGuild'] })).description, /Manage Expressions/);
  assert.match((await run({ strings: { emoji: 'pog' } }, fakeGuild({ botPerms: [] }))).description, /I need the \*\*Manage Expressions\*\*/);
  assert.match((await run({ strings: { emoji: 'dog' } })).description, /No emoji named/);
  assert.match((await run({ strings: { emoji: 'POG' } })).description, /Several emojis/);
  assert.match((await run({ strings: { emoji: '333333333333333333' } })).description, /managed by an integration/);
  assert.equal(guild.emojis.calls.delete.length, 0);
});

test('delete: confirm then delete posts a public result; cancel and timeout change nothing', async () => {
  const guild = fakeGuild({ emojis: [E('111111111111111111', 'pog'), E('222222222222222222', 'cat')] });
  const yes = fakeInteraction(guild, { sub: 'delete', strings: { emoji: '111111111111111111' }, confirm: { answer: 'yes' } });
  await m.execute(yes);
  assert.equal(yes.calls[0][0], 'reply');
  assert.equal(yes.calls[0][1].ephemeral, true, 'the confirm prompt is private');
  assert.deepEqual(guild.emojis.calls.delete, [{ id: '111111111111111111', reason: 'tester#0 (700000000000000001) via /emoji delete' }]);
  const done = lastCall(yes);
  assert.equal(done[0], 'followUp');
  assert.equal(done[1].ephemeral, undefined, 'the success is public');
  assert.match(embedOf(done).description, /`:pog:` was deleted/);
  assert.match(embedOf(done).description, /Static 1\/50/, 'the deleted id is excluded from the slot count');

  const no = fakeInteraction(guild, { sub: 'delete', strings: { emoji: '222222222222222222' }, confirm: { answer: 'no' } });
  await m.execute(no);
  assert.ok(!no.calls.some((c) => c[0] === 'followUp'), 'cancel posts no result');
  const timeout = fakeInteraction(guild, { sub: 'delete', strings: { emoji: '222222222222222222' }, confirm: { answer: null } });
  await m.execute(timeout);
  assert.ok(!timeout.calls.some((c) => c[0] === 'followUp'), 'timeout posts no result');
  assert.equal(guild.emojis.calls.delete.length, 1);
});

test('delete: an emoji removed during the confirm, and Discord errors, answer ephemerally', async () => {
  const guild = fakeGuild({ emojis: [E('111111111111111111', 'pog')] });
  const gone = fakeInteraction(guild, {
    sub: 'delete', strings: { emoji: 'pog' },
    confirm: { answer: 'yes', beforeAnswer: () => guild.emojis.cache.delete('111111111111111111') },
  });
  await m.execute(gone);
  const e = lastCall(gone);
  assert.equal(e[1].ephemeral, true);
  assert.equal(embedOf(e).title.includes('Already Deleted'), true);
  assert.equal(guild.emojis.calls.delete.length, 0);

  const denied = fakeGuild({
    emojis: [E('111111111111111111', 'pog')],
    remove: async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); },
  });
  const i = fakeInteraction(denied, { sub: 'delete', strings: { emoji: 'pog' }, confirm: { answer: 'yes' } });
  await m.execute(i);
  const d = lastCall(i);
  assert.equal(d[1].ephemeral, true);
  assert.equal(embedOf(d).title.includes('Delete Failed'), true);
  assert.match(embedOf(d).description, /Manage Expressions/);

  // With only Create Expressions the bot still tries (it may delete its own uploads).
  const createOnly = fakeGuild({ emojis: [E('111111111111111111', 'pog')], botPerms: ['CreateGuildExpressions'] });
  const j = fakeInteraction(createOnly, { sub: 'delete', strings: { emoji: 'pog' }, confirm: { answer: 'yes' } });
  await m.execute(j);
  assert.equal(createOnly.emojis.calls.delete.length, 1);
});
