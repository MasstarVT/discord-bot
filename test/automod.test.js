// pkg-automod tests (pkg-automod.md §10): config model, matchers, detectors, scope, outcome resolution, the plugin
// declaration, the /automod payload, the action pipeline with fakes, and (with TEST_DATABASE_URL) the /automod
// command end to end against a real settings row. Offline by default: no Discord, Postgres or Redis needed.
// DB cases: TEST_DATABASE_URL='<migrated db>' node --test --test-concurrency=1 test/automod.test.js
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only reached with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID; // the fakes must resolve as plain members

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';
// 18-digit ids with a package-unique prefix (955006 = this package's DB port), so the cleanup below never touches another package's rows.
const ID_PREFIX = '955006';

const svc = await import('../src/services/automodService.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { registerHook, clearHooks, listHooks } = await import('../src/handlers/hookRegistry.js');
const { getSettings, updateSettings } = await import('../src/services/settingsService.js');
const automod = await import('../src/commands/admin/automod.js');

const {
  RULE_KEYS, RULES, ACTIONS, DEFAULT_ALLOWED_DOMAINS, DELETING_ACTIONS, PUNISHMENTS, PUNISHMENT_RANK,
  normalizeConfig, upgradeV1, isLegacyConfig, defaultConfig, buildMatchers, getAutomodConfig, checkBadWords,
  findInvites, extractLinkHosts, hostMatches, normalizeDomain, normalizeWord, textForLinks, textForWords,
  capsStats, countEmojis, countSpoilers, countMentions, maxCombiningRun, duplicateFingerprint,
  scopeChannelIds, ruleApplies, resolveOutcome, buildReason, buildPunishmentLogFields, claimPunishment,
  resolveRuleKey, resetAutomodState, runAutomod, isStaff, evaluateContentRules, describeRuleLine, formatThreshold,
  buildNoticeText,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The real executeModAction (fake guild, real database) may record a case and create the settings row.
    const mine = { guildId: { startsWith: ID_PREFIX } };
    await prisma.infraction.deleteMany({ where: mine }).catch(() => {});
    await prisma.guildSettings.deleteMany({ where: mine }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});
beforeEach(() => resetAutomodState());

// Ids are 18 digits: the prefix plus a zero-padded counter.
const SNOW = (n) => `${ID_PREFIX}${String(n).padStart(12, '0')}`;
const GID = SNOW(1);
const UID = SNOW(2);
const OWNER = SNOW(3);
const BOT = SNOW(4);
const CH = SNOW(5);
const LOGCH = SNOW(6);

// ── 1-3. Config model ────────────────────────────────────────────────────────

test('normalizeConfig(null) is a full all-off v2 config', () => {
  const c = normalizeConfig(null);
  assert.equal(c.version, 2);
  assert.deepEqual(Object.keys(c.rules), [...RULE_KEYS]);
  for (const k of RULE_KEYS) {
    const r = c.rules[k];
    assert.equal(r.action, 'off', k);
    assert.equal(r.roleMode, 'except');
    assert.equal(r.channelMode, 'except');
    assert.deepEqual(r.roleIds, []);
    assert.deepEqual(r.channelIds, []);
    assert.equal(r.timeoutSeconds, 600);
    assert.equal('threshold' in r, RULES[k].threshold !== null, `${k} threshold presence`);
  }
  assert.deepEqual(c.allowedDomains, [...DEFAULT_ALLOWED_DOMAINS]);
  assert.equal(c.rules.caps.threshold, 70);
  assert.equal(c.rules.zalgo.threshold, 4);
  assert.deepEqual(c.badWords, []);
  assert.deepEqual(c.exemptRoleIds, []);
});

const FIXTURES = [
  'x', 42, [], { rules: 'x' },
  { version: 2, rules: { caps: { action: 'nuke', threshold: 'x', roleMode: 1 } }, badWords: null },
  { version: 2, badWords: [69, '', ' Foo ', 'foo', null, {}] },
  { version: 2, rules: { spam: { action: 'ban', threshold: 999, timeoutSeconds: 5, roleIds: ['1', '123456789012345678', 7] } } },
  { version: 2, allowedDomains: [], blockedDomains: ['Evil.COM', '', 'node', 7, 'evil.com'], exemptRoleIds: 'x' },
  { filterBadWords: true, badWords: ['a'] },
  undefined,
];

test('normalizeConfig never throws, returns a full v2 and is idempotent', () => {
  for (const f of FIXTURES) {
    const c = normalizeConfig(f);
    assert.equal(c.version, 2);
    assert.deepEqual(Object.keys(c.rules), [...RULE_KEYS]);
    assert.deepEqual(normalizeConfig(c), c, `idempotent for ${JSON.stringify(f)}`);
  }
  assert.deepEqual(normalizeConfig({ version: 2, badWords: [69, '', ' Foo ', 'foo', null, {}] }).badWords, ['69', 'foo']);
  const c = normalizeConfig(FIXTURES[4]);
  assert.equal(c.rules.caps.action, 'off');
  assert.equal(c.rules.caps.threshold, 70);
  assert.equal(c.rules.caps.roleMode, 'except');
  const s = normalizeConfig(FIXTURES[6]);
  assert.equal(s.rules.spam.threshold, 30, 'clamped to the maximum');
  assert.equal(s.rules.spam.timeoutSeconds, 60, 'clamped to the minimum');
  assert.deepEqual(s.rules.spam.roleIds, ['123456789012345678'], 'only snowflake strings survive');
  const d = normalizeConfig(FIXTURES[7]);
  assert.deepEqual(d.allowedDomains, [], 'an explicit [] stays empty');
  assert.deepEqual(d.blockedDomains, ['evil.com'], 'normalised, deduped, invalid dropped');
  assert.deepEqual(d.exemptRoleIds, []);
});

test('normalizeConfig does not mutate a frozen input and survives a hostile value', () => {
  const frozen = Object.freeze({ version: 2, rules: Object.freeze({ caps: Object.freeze({ action: 'delete', roleIds: Object.freeze([]) }) }), badWords: Object.freeze(['x1']) });
  const c = normalizeConfig(frozen);
  assert.equal(c.rules.caps.action, 'delete');
  c.badWords.push('mutable'); // the result is a fresh mutable object
  assert.deepEqual(frozen.badWords, ['x1']);
  const hostile = { version: 2, rules: { caps: { action: () => 1 } } };
  assert.doesNotThrow(() => normalizeConfig(hostile));
  assert.equal(normalizeConfig(hostile).rules.caps.action, 'off');
});

// ── 4-7. v1 upgrade ──────────────────────────────────────────────────────────

test('upgradeV1 maps only the keys that are present', () => {
  const u = upgradeV1({ filterInvites: true, automodAction: 'BAN' });
  assert.equal(u.rules.invites.action, 'ban');
  assert.equal(u.rules.mentions.action, 'off', 'a missing massMentionThreshold leaves mentions off');
  assert.equal(u.rules.links.action, 'off');
  assert.deepEqual(u.allowedDomains, [...DEFAULT_ALLOWED_DOMAINS]);
  assert.equal(upgradeV1({ filterLinks: true, automodAction: 'NONE' }).rules.links.action, 'delete');
  const m = upgradeV1({ filterLinks: true, automodAction: 'MUTE' });
  assert.equal(m.rules.links.action, 'timeout');
  assert.equal(m.rules.links.timeoutSeconds, 600);
  assert.equal(upgradeV1({ filterLinks: true }).rules.links.action, 'delete_warn');
  assert.equal(upgradeV1({ filterLinks: true, automodAction: 'KICK' }).rules.links.action, 'kick');
  assert.equal(upgradeV1({ filterLinks: true, automodAction: 'weird' }).rules.links.action, 'delete_warn');
});

test('upgradeV1 thresholds are clamped and the lists carry over', () => {
  const u = upgradeV1({ massMentionThreshold: 8, duplicateThreshold: 3, spamThreshold: 1 });
  assert.equal(u.rules.mentions.threshold, 8);
  assert.equal(u.rules.repeatedText.threshold, 3);
  assert.equal(u.rules.spam.threshold, 3, 'clamped up to the spam minimum');
  for (const k of ['mentions', 'repeatedText', 'spam']) assert.equal(u.rules[k].action, 'delete_warn', k);
  assert.equal(upgradeV1({ massMentionThreshold: 1 }).rules.mentions.threshold, 2);
  assert.equal(upgradeV1({ duplicateThreshold: 1 }).rules.repeatedText.action, 'off', 'duplicateThreshold < 2 is not a rule');
  const l = upgradeV1({ filterBadWords: true, badWords: ['Foo', null], blockedDomains: ['evil.com'], exemptChannelIds: [CH] });
  assert.deepEqual(l.badWords, ['foo']);
  assert.deepEqual(l.blockedDomains, ['evil.com']);
  assert.deepEqual(l.exemptChannelIds, [CH]);
});

test('isLegacyConfig and the empty upgrade', () => {
  assert.equal(isLegacyConfig({}), true);
  assert.equal(isLegacyConfig({ version: 2 }), false);
  assert.equal(isLegacyConfig(null), false);
  assert.equal(isLegacyConfig([]), false);
  assert.equal(isLegacyConfig('x'), false);
  const u = upgradeV1({});
  for (const k of RULE_KEYS) assert.equal(u.rules[k].action, 'off', k);
  assert.deepEqual(normalizeConfig({}), u);
});

// ── 8-9. Bad words and patterns ──────────────────────────────────────────────

test('bad words match whole words only (Scunthorpe, R034)', () => {
  const config = normalizeConfig({ version: 2, rules: { badWords: { action: 'delete' } }, badWords: ['ass', 'cunt', 'bad phrase'] });
  const matchers = buildMatchers(config);
  for (const ok of ['I passed the class', 'my assistant', 'Scunthorpe united', 'bassist', 'cunts_', '']) {
    assert.equal(checkBadWords(ok, config, matchers), null, ok);
  }
  const hits = [
    'you ass!', 'ASS', 'a\u{200B}ss', '||ass||', '\u{1D41A}\u{1D42C}\u{1D42C}', 'this is a bad   phrase', 'a**s**s', '*ass*', 'no, ass.',
  ];
  for (const text of hits) {
    const m = checkBadWords(text, config, matchers);
    assert.ok(m, `should match: ${JSON.stringify(text)}`);
    assert.equal(m.rule, 'badWords');
    assert.equal(m.action, 'delete');
  }
  assert.match(checkBadWords('you ass!', config, matchers).detail, /^word "ass"/);
});

test('bad-word patterns run safely and unsafe ones are dropped (R128)', () => {
  const config = normalizeConfig({ version: 2, badWordPatterns: ['fr[e3]{2}\\s*n[i1]tro', '(a+)+$', '(oops', 'x'.repeat(201)] });
  assert.equal(config.badWordPatterns.length, 3, 'the 201-character pattern is dropped by normalisation');
  const matchers = buildMatchers(config);
  assert.equal(matchers.patterns.length, 1, 'only the safe pattern compiles');
  assert.deepEqual(matchers.patternIndex, [0]);
  const m = checkBadWords('get FREE nitro now', config, matchers);
  assert.ok(m);
  assert.equal(m.detail, 'pattern #1');
  assert.equal(checkBadWords('hello there', config, matchers), null);
  assert.equal(buildMatchers(normalizeConfig(null)).badWordsRe, null);
});

test('a word list stays linear: a long non-matching message is fast', () => {
  const words = Array.from({ length: 400 }, (_, i) => `badword${i} x${i}`);
  const config = normalizeConfig({ version: 2, badWords: words });
  const matchers = buildMatchers(config);
  const text = 'badword '.repeat(500);
  const t0 = Date.now();
  assert.equal(checkBadWords(text, config, matchers), null);
  assert.ok(Date.now() - t0 < 1000);
});

// ── 10-12. Invites, links, domains ───────────────────────────────────────────

test('findInvites', () => {
  for (const s of ['discord.gg/abc', 'https://discord.com/invite/abc', 'discordapp.com/invite/abc', textForLinks('discord\\.gg/abc'), 'x https://www.discord.gg/Abc-12']) {
    assert.equal(findInvites(s).length, 1, s);
  }
  assert.equal(findInvites('https://discord.com/invite/AbC')[0], 'AbC', 'original case is kept');
  assert.deepEqual(findInvites('discord.game'), []);
  assert.deepEqual(findInvites('mydiscord.gg/abc'), []);
  assert.deepEqual(findInvites(null), []);
});

test('extractLinkHosts and hostMatches (R126, R127)', () => {
  for (const s of ['I went home.Then slept', 'use node.js here', 'see readme.md', 'Mr.Smith']) {
    assert.deepEqual(extractLinkHosts(s), [], s);
  }
  assert.deepEqual(extractLinkHosts('go https://evil.com/x).'), ['evil.com']);
  assert.deepEqual(extractLinkHosts('<https://evil.com>'), ['evil.com']);
  assert.deepEqual(extractLinkHosts('www.evil.com/path'), ['www.evil.com']);
  assert.deepEqual(extractLinkHosts('https://discord.gg/abc'), []);
  assert.deepEqual(extractLinkHosts('https://discord.com/invite/abc'), []);
  assert.deepEqual(extractLinkHosts('a https://a.com b https://a.com c https://b.org'), ['a.com', 'b.org']);
  assert.equal(hostMatches('netflix.com', 'x.com'), false);
  assert.equal(hostMatches('x.com', 'x.com'), true);
  assert.equal(hostMatches('a.x.com', 'x.com'), true);
});

test('normalizeDomain', () => {
  assert.equal(normalizeDomain('https://WWW.Example.com/path'), 'example.com');
  assert.equal(normalizeDomain('*.example.com'), 'example.com');
  assert.equal(normalizeDomain('Example.COM:8080/x?y#z'), 'example.com');
  for (const bad of ['', 'node', 'exa mple.com', null, undefined, '.com', 'a..b.com']) assert.equal(normalizeDomain(bad), null, String(bad));
  assert.equal(normalizeDomain('bücher.de'), 'xn--bcher-kva.de');
});

test('the links rule: allowlist mode, blocklist mode and the default allowed domains', () => {
  const env = (over) => {
    const config = normalizeConfig({ version: 2, rules: { links: { action: 'delete' } }, ...over });
    return { config, matchers: buildMatchers(config) };
  };
  const run = (e, content) => evaluateContentRules({ content }, ['links'], e.config, e.matchers, null);
  const allow = env({});
  assert.equal(run(allow, 'see https://evil.com/x').length, 1, 'no blocked domain: every non-allowed link is a violation');
  assert.equal(run(allow, 'gif https://tenor.com/view/abc').length, 0);
  assert.equal(run(allow, 'cdn https://cdn.discordapp.com/attachments/1/2/x.png').length, 0);
  assert.equal(run(allow, 'plain text with node.js').length, 0);
  assert.equal(run(allow, 'join https://discord.gg/abc').length, 0, 'invites belong to the invites rule');
  const block = env({ blockedDomains: ['evil.com'] });
  assert.equal(run(block, 'https://sub.evil.com/a').length, 1);
  assert.equal(run(block, 'https://good.com/a?u=evil.com').length, 0, 'only the hostname counts');
  assert.equal(run(block, 'https://notevil.com').length, 0);
});

// ── 13-18. Counters ──────────────────────────────────────────────────────────

test('capsStats', () => {
  assert.equal(capsStats('HELLO WORLD THIS IS LOUD').percent, 100);
  assert.equal(capsStats('OK').letters, 2);
  assert.equal(capsStats('<:LOUDEMOJI:123456789012345678> hi').letters, 2);
  assert.equal(capsStats('<@123456789012345678> https://EXAMPLE.COM/ABC hi').letters, 2);
  assert.equal(capsStats('').percent, 0);
  assert.equal(capsStats('Hello World').percent, 20);
});

test('countEmojis', () => {
  assert.equal(countEmojis('\u{1F600}'.repeat(3)), 3);
  assert.equal(countEmojis('\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}'), 1);
  assert.equal(countEmojis('\u{1F1EC}\u{1F1E7}'), 1);
  assert.equal(countEmojis('<:aa:123456789012345678><a:bb:123456789012345678>'), 2);
  assert.equal(countEmojis('<:a:123456789012345678>'), 0, 'custom emoji names are 2-32 characters');
  assert.equal(countEmojis('1 2 #'), 0);
  assert.equal(countEmojis('1\u{FE0F}\u{20E3}'), 1);
  assert.equal(countEmojis('hello'), 0);
});

test('countSpoilers', () => {
  assert.equal(countSpoilers('||a|| ||b|| ||c||', new Map()), 3);
  assert.equal(countSpoilers('||a|| ||b|| ||c||', new Map([['1', { spoiler: true }]])), 4);
  assert.equal(countSpoilers('', new Map([['1', { spoiler: true }], ['2', { spoiler: false }]])), 1);
  assert.equal(countSpoilers('||unclosed', undefined), 0);
});

test('countMentions uses parsed mentions (R274)', () => {
  const users = new Map([['a', {}], ['b', {}], ['author', {}]]);
  const message = {
    author: { id: 'author' }, content: 'hi',
    mentions: { users, roles: new Map([['r', {}]]), everyone: true },
  };
  assert.equal(countMentions(message), 4);
  const reply = { ...message, mentions: { ...message.mentions, repliedUser: { id: 'a' } } };
  assert.equal(countMentions(reply), 3, 'a reply ping without a typed mention is not counted');
  assert.equal(countMentions({ ...reply, content: 'hey <@a>' }), 4, 'a typed mention is');
  assert.equal(countMentions({ author: { id: 'x' }, content: '<@1> <@1> <@1>', mentions: { users: new Map(), roles: new Map(), everyone: false } }), 0);
  assert.equal(countMentions({}), 0);
});

test('maxCombiningRun', () => {
  assert.ok(maxCombiningRun('Z\u{353}\u{354}\u{355}\u{351}\u{352}o') >= 4);
  for (const ok of ['Tiếng Việt', 'Tie\u{302}\u{301}ng Vie\u{323}\u{302}t', 'क\u{94D}षत\u{94D}रिय', 'สว\u{E31}สด\u{E35}', '❤\u{FE0F}', '1\u{FE0F}\u{20E3}']) {
    assert.ok(maxCombiningRun(ok) < 4, JSON.stringify(ok));
  }
});

test('duplicateFingerprint (R035)', () => {
  assert.equal(duplicateFingerprint(''), null);
  assert.equal(duplicateFingerprint('  \u{200B} '), null);
  assert.equal(duplicateFingerprint(undefined), null);
  assert.equal(duplicateFingerprint('  Hello   World '), 'hello world');
});

test('textForWords and normalizeWord', () => {
  assert.equal(textForWords('b**a**d ||x|| \\_'), 'bad x _');
  assert.equal(normalizeWord(' Foo  Bar '), 'foo bar');
  assert.equal(normalizeWord('a'), null);
  assert.equal(normalizeWord('!!'), null);
  assert.equal(normalizeWord('x'.repeat(65)), null);
  assert.equal(normalizeWord('\u{1D41A}\u{1D42C}\u{1D42C}'), 'ass');
});

// ── 19-24. Scope, outcome, reasons ───────────────────────────────────────────

test('scopeChannelIds (R135)', () => {
  assert.deepEqual(scopeChannelIds({ id: 't', parentId: 'p', isThread: () => true, parent: { parentId: 'c' } }), ['t', 'p', 'c']);
  assert.deepEqual(scopeChannelIds({ id: 'x', parentId: 'c', isThread: () => false }), ['x', 'c']);
  assert.deepEqual(scopeChannelIds({ id: 'x', parentId: null, isThread: () => false }), ['x']);
  const viaCache = { id: 't', parentId: 'p', isThread: () => true, guild: { channels: { cache: new Map([['p', { parentId: 'c' }]]) } } };
  assert.deepEqual(scopeChannelIds(viaCache), ['t', 'p', 'c']);
  assert.deepEqual(scopeChannelIds(null), []);
});

test('ruleApplies', () => {
  const base = { roleMode: 'except', roleIds: [], channelMode: 'except', channelIds: [] };
  assert.equal(ruleApplies({ ...base, roleIds: ['r'] }, { roleIds: ['r'] }), false);
  assert.equal(ruleApplies({ ...base, roleMode: 'only', roleIds: ['r'] }, { roleIds: [] }), false);
  assert.equal(ruleApplies({ ...base, roleMode: 'only', roleIds: ['r'] }, { roleIds: ['r'] }), true);
  assert.equal(ruleApplies({ ...base, roleMode: 'only' }, { roleIds: ['r'] }), false, 'only + empty list applies to nobody');
  assert.equal(ruleApplies(base, { roleIds: ['r'], channelIds: ['c'] }), true);
  assert.equal(ruleApplies({ ...base, channelMode: 'only', channelIds: ['c'] }, { channelIds: ['x', 'c'] }), true);
  assert.equal(ruleApplies({ ...base, channelIds: ['c'] }, { channelIds: ['x', 'c'] }), false);
});

test('resolveOutcome', () => {
  const a = resolveOutcome([{ rule: 'caps', action: 'delete' }, { rule: 'invites', action: 'warn' }]);
  assert.equal(a.deletes, true);
  assert.equal(a.punishment, 'WARN');
  assert.equal(a.primary.rule, 'invites');
  assert.equal(a.punishmentNeedsDelete, false);
  assert.deepEqual(a.others.map((m) => m.rule), ['caps']);
  const b = resolveOutcome([{ rule: 'links', action: 'timeout', timeoutSeconds: 60 }, { rule: 'spam', action: 'timeout', timeoutSeconds: 600 }]);
  assert.equal(b.punishment, 'TIMEOUT');
  assert.equal(b.timeoutSeconds, 600);
  assert.equal(b.primary.rule, 'links', 'ties go to the first rule');
  const c = resolveOutcome([{ rule: 'badWords', action: 'delete_warn' }, { rule: 'invites', action: 'ban' }]);
  assert.equal(c.primary.rule, 'invites');
  assert.equal(c.punishment, 'BAN');
  assert.equal(c.punishmentNeedsDelete, true);
  const d = resolveOutcome([{ rule: 'caps', action: 'delete' }]);
  assert.equal(d.punishment, null);
  assert.equal(d.deletes, true);
  assert.equal(resolveOutcome([]), null);
  assert.equal(resolveOutcome(null), null);
});

test('buildReason stays within 500 characters (R036)', () => {
  const outcome = resolveOutcome([{ rule: 'badWords', action: 'delete_warn', detail: 'x'.repeat(5000) }, { rule: 'caps', action: 'delete', detail: 'y' }]);
  const reason = buildReason(outcome, 'general');
  assert.ok(reason.length <= 500);
  assert.ok(reason.startsWith('[Automod] '));
  const short = buildReason(resolveOutcome([{ rule: 'caps', action: 'delete', detail: '90% capitals' }]), 'general');
  assert.equal(short, '[Automod] Excessive Caps: 90% capitals in #general');
});

test('buildPunishmentLogFields: three entries, every value within 1024 (R275)', () => {
  const f = buildPunishmentLogFields('badWords', 'd'.repeat(5000), 'c'.repeat(5000), { id: CH });
  assert.equal(f.length, 3);
  assert.deepEqual(f.map((x) => x.name), ['Rule', 'Detail', 'Content']);
  for (const x of f) assert.ok(x.value.length >= 1 && x.value.length <= 1024, x.name);
  assert.equal(f[0].value, '\u{1F92C} Bad Words');
  assert.match(buildPunishmentLogFields('x', 'word "a"', 'hello', { id: CH })[1].value, new RegExp(`^word "a" in <#${CH}>$`));
  assert.equal(buildPunishmentLogFields('x', 'd', '', null)[2].value, '*[no text]*');
});

test('claimPunishment allows one punishment per window unless more severe', () => {
  assert.equal(claimPunishment('g', 'u', 1, 0), true);
  assert.equal(claimPunishment('g', 'u', 1, 1000), false);
  assert.equal(claimPunishment('g', 'u', 4, 1000), true);
  assert.equal(claimPunishment('g', 'u', 2, 2000), false, 'a lesser one after a ban is suppressed');
  assert.equal(claimPunishment('g', 'u', 1, 30_999), false, 'the ban window runs to 31 000');
  assert.equal(claimPunishment('g', 'u', 1, 31_000), true);
  assert.equal(claimPunishment('g', 'other', 1, 1000), true, 'per member');
  assert.equal(claimPunishment('g2', 'u', 1, 1000), true, 'per guild');
});

test('resolveRuleKey and the action tables', () => {
  assert.equal(resolveRuleKey('caps'), 'caps');
  assert.equal(resolveRuleKey('Excessive Caps'), 'caps');
  assert.equal(resolveRuleKey('anti-spam'), 'spam');
  assert.equal(resolveRuleKey('Caps (caps)'), 'caps');
  assert.equal(resolveRuleKey('\u{1F520} Excessive Caps (caps)'), 'caps');
  assert.equal(resolveRuleKey('BADWORDS'), 'badWords');
  assert.equal(resolveRuleKey('duplicates'), 'repeatedText');
  assert.equal(resolveRuleKey('nope'), null);
  assert.equal(resolveRuleKey(''), null);
  assert.equal(resolveRuleKey(undefined), null);
  assert.equal(resolveRuleKey('__proto__'), null);
  for (const key of RULE_KEYS) assert.equal(resolveRuleKey(key), key);
  assert.equal(RULE_KEYS.length, 10);
  assert.equal(ACTIONS.length, 7);
  for (const action of ACTIONS) {
    const punishes = action in PUNISHMENTS;
    assert.equal(DELETING_ACTIONS.has(action), !['off', 'warn'].includes(action), action);
    if (punishes) assert.ok(PUNISHMENT_RANK[PUNISHMENTS[action]] >= 1);
  }
  assert.ok(Object.isFrozen(RULES) && Object.isFrozen(RULES.caps));
});

test('describeRuleLine and formatThreshold', () => {
  const config = normalizeConfig({ version: 2, badWords: ['aa'], rules: {
    caps: { action: 'timeout', threshold: 80, timeoutSeconds: 7200, roleMode: 'only', roleIds: [UID], channelIds: [CH, LOGCH] },
    badWords: { action: 'delete' },
  } });
  const caps = describeRuleLine('caps', config.rules.caps, config);
  assert.match(caps, /Excessive Caps\*\* — Delete & timeout · ≥ 80% capitals · 2 hours · roles: only 1 · channels: except 2/);
  assert.match(describeRuleLine('badWords', config.rules.badWords, config), /Delete message · 1 words, 0 patterns$/);
  assert.equal(describeRuleLine('emojis', config.rules.emojis, config), '\u{1F600} **Excessive Emojis** — Off');
  assert.equal(formatThreshold('spam', 6), '6 messages in 5 s');
  assert.equal(formatThreshold('repeatedText', 3), '3 identical messages in 30 s');
});

test('getAutomodConfig memoises per automodConfig object', () => {
  const raw = Object.freeze({ version: 2, rules: { caps: { action: 'delete' } } });
  const a = getAutomodConfig({ automodConfig: raw });
  const b = getAutomodConfig({ automodConfig: raw });
  assert.equal(a, b);
  assert.equal(a.config.rules.caps.action, 'delete');
  const next = getAutomodConfig({ automodConfig: { ...raw } });
  assert.notEqual(next, a, 'a new object recompiles');
  const none = getAutomodConfig(null);
  assert.equal(none, getAutomodConfig({ automodConfig: null }));
  assert.deepEqual(none.config, defaultConfig());
});

// ── 25-26. Command payload and plugin ────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};
const everyOption = (node, fn) => { for (const o of node.options ?? []) { fn(o); everyOption(o, fn); } };

test('the /automod payload fits Discord limits', () => {
  const json = buildCommandPayload(automod);
  assert.equal(json.name, 'automod');
  assert.ok(charCount(json) <= 2600, `charCount ${charCount(json)}`);
  assert.equal(json.options.length, 8);
  everyOption(json, (o) => assert.ok(o.description.length >= 1 && o.description.length <= 100, `${o.name} description`));
  assert.equal(automod.module, 'automod');
  assert.equal(automod.gated, false);
  assert.equal(automod.permissionLevel, 2);
  assert.deepEqual(json.options.map((o) => o.name), ['enable', 'disable', 'status', 'rule', 'words', 'patterns', 'domains', 'exempt']);
  const rule = json.options.find((o) => o.name === 'rule');
  assert.deepEqual(rule.options.map((o) => o.name), ['rule', 'action', 'threshold', 'timeout', 'roles', 'channels']);
  assert.equal(rule.options[0].autocomplete, true);
  assert.equal(rule.options[1].choices.length, 7);
  assert.equal(typeof automod.autocomplete, 'function');
});

test('the plugin declares both hooks and registers them first / after logging', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /automod/.test(e)), []);
  const p = plugins.find((x) => x.id === 'automod');
  assert.ok(p, 'the automod plugin is collected');
  assert.equal(p.module, 'automod');
  assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}`).sort(), ['messageCreate:automod', 'messageUpdate:automod']);
  assert.deepEqual(p.components, []);
  assert.deepEqual(p.jobs, []);
  clearHooks();
  try {
    for (const pl of plugins) for (const h of pl.hooks) registerHook(h, { pluginId: pl.id, pluginModule: pl.module });
    assert.equal(listHooks('messageCreate')[0].id, 'automod');
    const upd = listHooks('messageUpdate').map((h) => h.id);
    assert.ok(upd.indexOf('automod') >= 0);
    if (upd.includes('logging')) assert.ok(upd.indexOf('logging') < upd.indexOf('automod'));
    assert.equal(listHooks('messageCreate').find((h) => h.id === 'automod').module, 'automod');
  } finally {
    clearHooks();
  }
  // The filter skips bots and messages without a guild.
  const filter = p.hooks[0].filter;
  assert.equal(filter({ guild: {}, message: { author: { bot: false } } }), true);
  assert.equal(filter({ guild: {}, message: { author: { bot: true } } }), false);
  assert.equal(filter({ guild: null, message: { author: {} } }), false);
  assert.equal(filter({ guild: {}, message: null }), false);
});

test('static scan: the owned source files avoid forbidden patterns', () => {
  const FORBIDDEN = /console\.|prisma|createInfraction|sendModLog|loggingService|cache\.del\(|new RegExp\(pattern|pushRecent/;
  for (const file of ['src/services/automodService.js', 'src/plugins/automod.js', 'src/commands/admin/automod.js']) {
    const text = readFileSync(resolve(ROOT, file), 'utf8');
    const m = text.match(FORBIDDEN);
    assert.equal(m, null, `${file} contains ${m?.[0]}`);
    assert.equal(/\bsetTimeout\(|\bsetInterval\(/.test(text.split('\n').filter((l) => !/^\s/.test(l)).join('\n')), false, `${file}: no timer at module scope`);
  }
  const service = readFileSync(resolve(ROOT, 'src/services/automodService.js'), 'utf8');
  assert.equal((service.match(/new RegExp\(/g) ?? []).length, 1, 'the only new RegExp( is the escaped-word builder');
});

// ── 27-28 and more: the pipeline with fakes (no Redis, no database) ──────────

function makeEnv({
  content = 'join discord.gg/abc',
  rules = { invites: { action: 'delete' } },
  config,
  canDelete = true,
  deleteError = null,
  memberPerms = false,
  memberRoles = [],
  settings = {},
  modLog = false,
  channelHasGuild = true,
  channelOver = {},
  mentions,
  attachments = new Map(),
  memberFetch,
  noMember = false,
  oldContent,
  isEdit = false,
  authorId = UID,
} = {}) {
  const sent = [];
  const logSent = [];
  const deletes = [];
  const stops = [];
  const guild = {
    id: GID,
    ownerId: OWNER,
    available: true,
    members: {
      me: { id: BOT },
      fetch: memberFetch ?? (async () => { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); }),
    },
    invites: { cache: new Map() },
    vanityURLCode: null,
    channels: { cache: new Map() },
  };
  const channel = {
    id: CH,
    name: 'general',
    parentId: null,
    isThread: () => false,
    isTextBased: () => true,
    permissionsFor: () => ({ has: () => canDelete }),
    send: async (payload) => { sent.push(payload); return { id: SNOW(50 + sent.length), delete: async () => {} }; },
    ...channelOver,
  };
  if (channelHasGuild) channel.guild = guild;
  if (modLog) {
    guild.channels.cache.set(LOGCH, {
      id: LOGCH, guild, isTextBased: () => true, isThread: () => false, permissionsFor: () => ({ has: () => true }),
      send: async (payload) => { logSent.push(payload); return {}; },
    });
  }
  const member = {
    id: authorId,
    user: { id: authorId, bot: false },
    guild,
    permissions: { has: () => memberPerms },
    roles: { cache: new Map(memberRoles.map((r) => [r, { id: r }])), highest: { position: 0 } },
  };
  const message = {
    id: SNOW(100 + Math.floor(Math.random() * 1e6)),
    content,
    author: { id: authorId, bot: false, username: 'someone', tag: 'someone#0' },
    member: noMember ? null : member,
    channel,
    channelId: CH,
    url: `https://discord.com/channels/${GID}/${CH}/1`,
    attachments,
    mentions: mentions ?? { users: new Map(), roles: new Map(), everyone: false },
    delete: async () => { deletes.push(1); if (deleteError) throw deleteError; },
  };
  const settingsRow = {
    automodEnabled: true,
    automodConfig: config ?? { version: 2, rules },
    modLogChannelId: modLog ? LOGCH : null,
    modRoleIds: [],
    immuneRoleIds: [],
    ...settings,
  };
  const ctx = {
    guild,
    guildId: GID,
    client: { user: { id: BOT } },
    message,
    member: noMember ? null : member,
    getSettings: async () => settingsRow,
    stop: (reason) => stops.push(reason),
    state: {},
  };
  if (isEdit) {
    ctx.oldKnown = oldContent !== undefined;
    ctx.oldMessage = oldContent !== undefined ? { ...message, content: oldContent } : null;
  }
  return { ctx, guild, channel, member, message, sent, logSent, deletes, stops };
}

test('pipeline: an invite is deleted, the hook chain stops and the state is set', async () => {
  const env = makeEnv({ modLog: true });
  await runAutomod(env.ctx, { isEdit: false });
  assert.equal(env.deletes.length, 1);
  assert.deepEqual(env.stops, ['automod']);
  assert.deepEqual(env.ctx.state.automodAction, { rule: 'invites', deleted: true });
  assert.equal(env.sent.length, 1, 'one in-channel notice');
  assert.match(env.sent[0].content, /your message was removed: \*\*Server Invites\*\*/);
  assert.deepEqual(env.sent[0].allowedMentions, { users: [env.message.author.id] });
  assert.equal(env.logSent.length, 1, 'exactly one mod-log embed (R275)');
  const embed = env.logSent[0].embeds[0].toJSON();
  assert.match(embed.title, /Message Deleted/);
  assert.deepEqual(env.logSent[0].allowedMentions, { parse: [] });
  assert.deepEqual(embed.fields.map((f) => f.name), ['Member', 'Channel', 'Rule', 'Detail', 'Content']);
  for (const f of embed.fields) assert.ok(f.value.length <= 1024);
  assert.match(embed.footer.text, /Further removals for this member and rule are not logged for 10 s/);
});

test('pipeline: a burst logs one delete embed per member and rule, but deletes every message', async () => {
  const first = makeEnv({ modLog: true });
  await runAutomod(first.ctx);
  const second = makeEnv({ modLog: true });
  await runAutomod(second.ctx);
  assert.equal(first.deletes.length + second.deletes.length, 2);
  assert.equal(first.logSent.length, 1);
  assert.equal(second.logSent.length, 0, 'throttled');
  assert.equal(second.sent.length, 0, 'one notice per member per 10 s');
  assert.deepEqual(second.stops, ['automod']);
});

test('pipeline: no Manage Messages means no delete, no stop and one NOT deleted embed (R132)', async () => {
  const env = makeEnv({ canDelete: false, modLog: true, rules: { invites: { action: 'delete_warn' } } });
  await runAutomod(env.ctx);
  assert.equal(env.deletes.length, 0);
  assert.deepEqual(env.stops, []);
  assert.deepEqual(env.ctx.state.automodAction, { rule: 'invites', deleted: false });
  assert.equal(env.sent.length, 0, 'no "removed" notice');
  assert.equal(env.logSent.length, 1);
  const embed = env.logSent[0].embeds[0].toJSON();
  assert.match(embed.title, /Message NOT Deleted/);
  assert.match(embed.fields.at(-1).value, /Manage Messages/);
  assert.match(embed.fields.at(-1).value, /No punishment was applied/);
  const again = makeEnv({ canDelete: false, modLog: true, rules: { invites: { action: 'delete_warn' } } });
  await runAutomod(again.ctx);
  assert.equal(again.logSent.length, 0, 'one NOT deleted embed per channel per minute');
});

test('pipeline: Discord delete errors (R132)', async () => {
  const gone = makeEnv({ deleteError: Object.assign(new Error('Unknown Message'), { code: 10008 }), modLog: true });
  await runAutomod(gone.ctx);
  assert.deepEqual(gone.stops, ['automod'], 'already gone counts as deleted');
  assert.equal(gone.ctx.state.automodAction.deleted, true);
  assert.equal(gone.sent.length, 0, 'no notice for a message somebody else removed');
  const denied = makeEnv({ deleteError: Object.assign(new Error('Missing Permissions'), { code: 50013 }), modLog: true });
  resetAutomodState();
  await runAutomod(denied.ctx);
  assert.deepEqual(denied.stops, []);
  assert.equal(denied.ctx.state.automodAction.deleted, false);
  assert.match(denied.logSent[0].embeds[0].toJSON().title, /NOT Deleted/);
  resetAutomodState();
  const odd = makeEnv({ deleteError: Object.assign(new Error('boom'), { code: 500 }), modLog: true });
  await runAutomod(odd.ctx);
  assert.deepEqual(odd.stops, []);
  assert.match(odd.logSent[0].embeds[0].toJSON().fields.at(-1).value, /Deleting failed/);
});

test('pipeline: staff, the owner, exempt roles and exempt channels are skipped (R134, R135)', async () => {
  const staff = [
    makeEnv({ memberPerms: true }), // Manage Messages / Administrator / Kick / Ban ... all answer true
    makeEnv({ authorId: OWNER }),
    makeEnv({ settings: { modRoleIds: [SNOW(70)] }, memberRoles: [SNOW(70)] }),
    makeEnv({ settings: { immuneRoleIds: [SNOW(71)] }, memberRoles: [SNOW(71)] }),
    makeEnv({ config: { version: 2, rules: { invites: { action: 'delete' } }, exemptRoleIds: [SNOW(72)] }, memberRoles: [SNOW(72)] }),
    makeEnv({ config: { version: 2, rules: { invites: { action: 'delete' } }, exemptChannelIds: [CH] } }),
    makeEnv({ config: { version: 2, rules: { invites: { action: 'delete' } }, exemptChannelIds: [SNOW(73)] }, channelOver: { parentId: SNOW(73) } }),
    makeEnv({ config: { version: 2, rules: { invites: { action: 'delete', roleIds: [SNOW(74)] } } }, memberRoles: [SNOW(74)] }),
    makeEnv({ config: { version: 2, rules: { invites: { action: 'delete', channelMode: 'only', channelIds: [SNOW(75)] } } } }),
  ];
  for (const [i, env] of staff.entries()) {
    await runAutomod(env.ctx);
    assert.equal(env.deletes.length, 0, `case ${i} must not delete`);
    assert.deepEqual(env.stops, [], `case ${i} must not stop`);
    assert.equal(env.ctx.state.automodAction, undefined, `case ${i}`);
  }
  // A thread inside an exempt category is exempt too.
  const thread = makeEnv({
    config: { version: 2, rules: { invites: { action: 'delete' } }, exemptChannelIds: [SNOW(76)] },
    channelOver: { isThread: () => true, parentId: SNOW(77), parent: { parentId: SNOW(76) } },
  });
  await runAutomod(thread.ctx);
  assert.equal(thread.deletes.length, 0);
  // Control: the same member with a role-scoped "only" rule is moderated when they hold the role.
  const only = makeEnv({
    config: { version: 2, rules: { invites: { action: 'delete', roleMode: 'only', roleIds: [SNOW(78)] } } },
    memberRoles: [SNOW(78)],
  });
  await runAutomod(only.ctx);
  assert.equal(only.deletes.length, 1);
});

test('pipeline: isStaff', () => {
  const guild = { ownerId: OWNER };
  const member = (over) => ({ id: UID, guild, permissions: { has: () => false }, roles: { cache: new Map() }, ...over });
  assert.equal(isStaff(null, {}), false);
  assert.equal(isStaff(member(), { modRoleIds: [], immuneRoleIds: [] }), false);
  assert.equal(isStaff(member({ id: OWNER }), {}), true);
  assert.equal(isStaff(member({ permissions: { has: () => true } }), {}), true);
  assert.equal(isStaff(member({ roles: { cache: new Map([['m', {}]]) } }), { modRoleIds: ['m'] }), true);
  assert.equal(isStaff(member({ permissions: null, roles: null }), null), false);
  assert.equal(isStaff({ get guild() { throw new Error('boom'); } }, {}), true, 'fails safe: an error counts as staff');
});

test('pipeline: disabled automod, bots and system messages do nothing', async () => {
  const off = makeEnv({ settings: { automodEnabled: false } });
  await runAutomod(off.ctx);
  assert.equal(off.deletes.length, 0);
  const noRules = makeEnv({ rules: {} });
  await runAutomod(noRules.ctx);
  assert.equal(noRules.deletes.length, 0);
  const bot = makeEnv();
  bot.message.author.bot = true;
  await runAutomod(bot.ctx);
  assert.equal(bot.deletes.length, 0);
  const sys = makeEnv();
  sys.message.system = true;
  await runAutomod(sys.ctx);
  assert.equal(sys.deletes.length, 0);
  const hook = makeEnv();
  hook.message.webhookId = SNOW(9);
  await runAutomod(hook.ctx);
  assert.equal(hook.deletes.length, 0);
  const nullSettings = makeEnv();
  nullSettings.ctx.getSettings = async () => null;
  await runAutomod(nullSettings.ctx);
  assert.equal(nullSettings.deletes.length, 0);
  await runAutomod(undefined);
  await runAutomod({});
});

test('pipeline: a malformed config never throws and never deletes by accident (R273)', async () => {
  for (const automodConfig of ['x', 42, [], { rules: 'x' }, { version: 2, badWords: [null, '', 7, {}], blockedDomains: ['', null], badWordPatterns: ['', 5] }, { filterBadWords: true, badWords: [''] }]) {
    const env = makeEnv({ config: automodConfig, content: 'hello world, nothing wrong here' });
    await assert.doesNotReject(runAutomod(env.ctx));
    assert.equal(env.deletes.length, 0, JSON.stringify(automodConfig));
  }
  const throws = makeEnv();
  throws.ctx.getSettings = async () => { throw new Error('db down'); };
  await assert.doesNotReject(runAutomod(throws.ctx));
});

test('pipeline: an unknown member is handled safely', async () => {
  // The member left (10007): the message is deleted, nobody is punished.
  const left = makeEnv({ noMember: true, modLog: true, rules: { invites: { action: 'delete_warn' } } });
  await runAutomod(left.ctx);
  assert.equal(left.deletes.length, 1);
  assert.deepEqual(left.stops, ['automod']);
  assert.match(left.logSent[0].embeds[0].toJSON().title, /Warn Not Applied/);
  assert.match(left.logSent[0].embeds[0].toJSON().fields.at(-1).value, /no longer in the server/);
  // A transient lookup failure proves nothing about staff status: do nothing.
  const flaky = makeEnv({
    noMember: true,
    memberFetch: async () => { throw Object.assign(new Error('rate limited'), { code: 429 }); },
  });
  await runAutomod(flaky.ctx);
  assert.equal(flaky.deletes.length, 0);
  assert.deepEqual(flaky.stops, []);
  // A fetched member who is staff is skipped.
  const fetchedStaff = makeEnv({ noMember: true });
  fetchedStaff.guild.members.fetch = async () => ({ ...fetchedStaff.member, permissions: { has: () => true } });
  await runAutomod(fetchedStaff.ctx);
  assert.equal(fetchedStaff.deletes.length, 0);
});

test('pipeline: a failed punishment is reported once and the message is still removed', async () => {
  // The fake guild has no ban list and no permissions, so executeModAction reports ok: false (it never throws),
  // with or without a database behind it.
  const env = makeEnv({ modLog: true, rules: { invites: { action: 'ban' } } });
  await runAutomod(env.ctx);
  assert.equal(env.deletes.length, 1);
  assert.deepEqual(env.stops, ['automod']);
  assert.equal(env.logSent.length, 1);
  const embed = env.logSent[0].embeds[0].toJSON();
  assert.match(embed.title, /Message Deleted, Ban Not Applied/);
  assert.match(embed.fields.at(-1).value, /^[A-Z_]+: /);
  assert.match(env.sent[0].content, /your message was removed: /, 'a failed punishment is not announced');
  // A second message in the window: the slot is taken, so the punishment is suppressed with no embed.
  const second = makeEnv({ modLog: true, rules: { invites: { action: 'ban' } } });
  await runAutomod(second.ctx);
  assert.equal(second.deletes.length, 1);
  assert.equal(second.logSent.length, 0);
});

test('pipeline: a warn-only action keeps the message and never stops the chain', async () => {
  const env = makeEnv({ modLog: true, rules: { invites: { action: 'warn' } } });
  await runAutomod(env.ctx);
  assert.equal(env.deletes.length, 0);
  assert.deepEqual(env.stops, []);
  assert.deepEqual(env.ctx.state.automodAction, { rule: 'invites', deleted: false });
  assert.ok(env.logSent.length <= 1, 'at most one embed: a failure embed, or none when moderation recorded the warning itself');
});

test('pipeline: mentions, caps, emojis, spoilers and zalgo rules', async () => {
  const many = new Map(Array.from({ length: 6 }, (_, i) => [SNOW(200 + i), {}]));
  const mentions = makeEnv({ content: '<@1> hi', rules: { mentions: { action: 'delete', threshold: 5 } }, mentions: { users: many, roles: new Map(), everyone: false } });
  await runAutomod(mentions.ctx);
  assert.equal(mentions.deletes.length, 1);
  assert.equal(mentions.ctx.state.automodAction.rule, 'mentions');
  const few = makeEnv({ content: 'hi', rules: { mentions: { action: 'delete', threshold: 5 } }, mentions: { users: new Map([['a', {}]]), roles: new Map(), everyone: false } });
  await runAutomod(few.ctx);
  assert.equal(few.deletes.length, 0);
  const caps = makeEnv({ content: 'THIS IS ALL CAPITALS HERE', rules: { caps: { action: 'delete' } } });
  await runAutomod(caps.ctx);
  assert.equal(caps.deletes.length, 1);
  const shortCaps = makeEnv({ content: 'OK THANKS', rules: { caps: { action: 'delete' } } });
  await runAutomod(shortCaps.ctx);
  assert.equal(shortCaps.deletes.length, 0, 'fewer than 10 letters');
  const emojis = makeEnv({ content: '\u{1F600}'.repeat(10), rules: { emojis: { action: 'delete' } } });
  await runAutomod(emojis.ctx);
  assert.equal(emojis.deletes.length, 1);
  const spoilerFiles = makeEnv({ content: '', rules: { spoilers: { action: 'delete', threshold: 2 } }, attachments: new Map([['1', { spoiler: true }], ['2', { spoiler: true }]]) });
  await runAutomod(spoilerFiles.ctx);
  assert.equal(spoilerFiles.deletes.length, 1, 'an empty message still counts spoiler attachments');
  const emptyCaps = makeEnv({ content: '', rules: { caps: { action: 'delete' }, badWords: { action: 'delete' } }, config: { version: 2, rules: { caps: { action: 'delete' }, badWords: { action: 'delete' } }, badWords: ['aa'] } });
  await runAutomod(emptyCaps.ctx);
  assert.equal(emptyCaps.deletes.length, 0);
  const zalgo = makeEnv({ content: 'Z\u{353}\u{354}\u{355}\u{351}\u{352}o', rules: { zalgo: { action: 'delete' } } });
  await runAutomod(zalgo.ctx);
  assert.equal(zalgo.deletes.length, 1);
});

test('pipeline: bad words and links through runAutomod', async () => {
  const config = { version: 2, rules: { badWords: { action: 'delete' }, links: { action: 'delete' } }, badWords: ['ass'], blockedDomains: ['evil.com'] };
  for (const [content, expected] of [['you ass', 1], ['my assistant', 0], ['see https://evil.com/x', 1], ['see https://fine.org/x', 0], ['go to node.js', 0]]) {
    const env = makeEnv({ content, config });
    await runAutomod(env.ctx);
    assert.equal(env.deletes.length, expected, content);
  }
  // This guild's own invite (vanity code or cached invite) is not a violation.
  const own = makeEnv({ content: 'join discord.gg/mine' });
  own.guild.vanityURLCode = 'Mine';
  await runAutomod(own.ctx);
  assert.equal(own.deletes.length, 0);
  const cached = makeEnv({ content: 'join discord.gg/abc123' });
  cached.guild.invites.cache.set('abc123', {});
  await runAutomod(cached.ctx);
  assert.equal(cached.deletes.length, 0);
});

test('pipeline: several matches act once with the strongest action', async () => {
  const env = makeEnv({
    content: 'DISCORD.GG/ABC NOW',
    config: { version: 2, rules: { invites: { action: 'delete' }, caps: { action: 'delete' } } },
  });
  await runAutomod(env.ctx);
  assert.equal(env.deletes.length, 1, 'one delete for two matching rules');
  assert.equal(env.stops.length, 1);
  assert.equal(env.ctx.state.automodAction.rule, 'invites');
});

test('edits: only the content rules run, and a rule the old text already broke is not repeated (R119)', async () => {
  const fresh = makeEnv({ isEdit: true, oldContent: 'harmless', modLog: true });
  await runAutomod(fresh.ctx, { isEdit: true });
  assert.equal(fresh.deletes.length, 1);
  assert.deepEqual(fresh.stops, ['automod']);
  assert.match(fresh.logSent[0].embeds[0].toJSON().fields[1].value, /\(edited message\)/);
  const repeat = makeEnv({ isEdit: true, oldContent: 'join discord.gg/abc please', content: 'join discord.gg/abc thanks' });
  await runAutomod(repeat.ctx, { isEdit: true });
  assert.equal(repeat.deletes.length, 0, 'the old content already broke the rule');
  assert.deepEqual(repeat.stops, []);
  const unknownOld = makeEnv({ isEdit: true });
  await runAutomod(unknownOld.ctx, { isEdit: true });
  assert.equal(unknownOld.deletes.length, 1, 'an uncached old message cannot excuse the edit');
  const spamOnEdit = makeEnv({ isEdit: true, oldContent: 'x', content: 'x', rules: { spam: { action: 'delete', threshold: 3 }, repeatedText: { action: 'delete', threshold: 2 } } });
  await runAutomod(spamOnEdit.ctx, { isEdit: true });
  assert.equal(spamOnEdit.deletes.length, 0, 'spam and repeatedText never run on edits');
});

test('pipeline: with Redis down, spam and repeatedText fail open', async () => {
  const env = makeEnv({
    content: 'same thing',
    rules: { spam: { action: 'ban', threshold: 3 }, repeatedText: { action: 'ban', threshold: 2 } },
  });
  await runAutomod(env.ctx);
  assert.equal(env.deletes.length, 0);
  assert.deepEqual(env.stops, []);
  assert.equal(env.logSent.length, 0);
});

// The cache is a plain object, so its Redis methods can be replaced for one test.
function fakeWindow({ available = true, nullCount = false } = {}) {
  const sets = new Map();
  const calls = [];
  return {
    calls,
    available: () => available,
    slidingWindow: async (key, windowMs, member) => {
      calls.push({ key, windowMs, member });
      if (nullCount) return null;
      const set = sets.get(key) ?? new Set();
      set.add(member);
      sets.set(key, set);
      return set.size;
    },
  };
}

async function withFakeRedis(impl, fn) {
  const original = { available: cache.available, slidingWindow: cache.slidingWindow };
  cache.available = impl.available;
  cache.slidingWindow = impl.slidingWindow;
  try {
    await fn(impl);
  } finally {
    Object.assign(cache, original);
  }
}

test('anti-spam: a sliding window per member, atomic and keyed by message id (R205, R129, R131)', async () => {
  await withFakeRedis(fakeWindow(), async (redisFake) => {
    const rules = { spam: { action: 'delete', threshold: 3 } };
    const envs = ['a', 'b', 'c', 'd'].map((c) => makeEnv({ content: c, rules }));
    for (const env of envs) await runAutomod(env.ctx);
    assert.deepEqual(envs.map((e) => e.deletes.length), [0, 0, 1, 1], 'the third message in the window and every later one');
    assert.equal(redisFake.calls.length, 4);
    for (const [i, call] of redisFake.calls.entries()) {
      assert.equal(call.key, `automod:rate:${GID}:${UID}`);
      assert.equal(call.windowMs, 5000);
      assert.equal(call.member, envs[i].message.id, 're-delivered events are never counted twice');
    }
    assert.equal(envs[2].ctx.state.automodAction.rule, 'spam');
    // Attachment-only and sticker-only messages count too.
    const media = makeEnv({ content: '', rules, attachments: new Map([['1', {}]]) });
    await runAutomod(media.ctx);
    assert.equal(redisFake.calls.length, 5);
  });
});

test('repeated text: one window per member and content hash; empty content never counts (R035, R130)', async () => {
  await withFakeRedis(fakeWindow(), async (redisFake) => {
    const rules = { repeatedText: { action: 'delete', threshold: 2 } };
    const first = makeEnv({ content: 'Buy   now', rules });
    const second = makeEnv({ content: 'buy NOW', rules });
    const other = makeEnv({ content: 'something else', rules });
    for (const env of [first, second, other]) await runAutomod(env.ctx);
    assert.deepEqual([first, second, other].map((e) => e.deletes.length), [0, 1, 0]);
    assert.match(redisFake.calls[0].key, new RegExp(`^automod:dup:${GID}:${UID}:[0-9a-f]{16}$`));
    assert.equal(redisFake.calls[0].key, redisFake.calls[1].key, 'case and spacing do not change the fingerprint');
    assert.notEqual(redisFake.calls[0].key, redisFake.calls[2].key);
    assert.equal(redisFake.calls[0].windowMs, 30_000);
    const before = redisFake.calls.length;
    const image = makeEnv({ content: '', rules, attachments: new Map([['1', {}]]) });
    await runAutomod(image.ctx);
    const blank = makeEnv({ content: '   ', rules });
    await runAutomod(blank.ctx);
    assert.equal(redisFake.calls.length, before, 'no window is touched for empty content');
    assert.equal(image.deletes.length + blank.deletes.length, 0);
  });
});

test('redis rules fail open and are never fed by staff, out-of-scope members or edits', async () => {
  const rules = { spam: { action: 'delete', threshold: 3 }, repeatedText: { action: 'delete', threshold: 2 } };
  await withFakeRedis(fakeWindow({ nullCount: true }), async (redisFake) => {
    const env = makeEnv({ rules });
    await runAutomod(env.ctx);
    assert.equal(redisFake.calls.length, 2);
    assert.equal(env.deletes.length, 0, 'a null count (Redis down) never punishes');
  });
  await withFakeRedis(fakeWindow({ available: false }), async (redisFake) => {
    const env = makeEnv({ rules });
    await runAutomod(env.ctx);
    assert.equal(redisFake.calls.length, 0, 'Redis reported down: the windows are not even tried');
    assert.equal(env.deletes.length, 0);
  });
  await withFakeRedis(fakeWindow(), async (redisFake) => {
    await runAutomod(makeEnv({ rules, memberPerms: true }).ctx);
    await runAutomod(makeEnv({ rules: { spam: { action: 'delete', threshold: 3, roleMode: 'only', roleIds: [SNOW(90)] } } }).ctx);
    await runAutomod(makeEnv({ rules, isEdit: true, oldContent: 'x' }).ctx, { isEdit: true });
    assert.equal(redisFake.calls.length, 0);
  });
});

test('buildNoticeText: wording per outcome, never echoing the matched word', () => {
  const author = { id: UID, username: 'someone' };
  const t = (over) => buildNoticeText({ deleted: true, status: null, punishment: null, author, label: 'Bad Words', ...over });
  assert.deepEqual(t({}), { text: `\u{1F6E1}\u{FE0F} <@${UID}>, your message was removed: **Bad Words**.`, mention: true });
  assert.equal(t({ status: 'applied', punishment: 'WARN' }).text, `\u{1F6E1}\u{FE0F} <@${UID}>, your message was removed and you have been warned: **Bad Words**.`);
  assert.equal(t({ status: 'applied', punishment: 'TIMEOUT', timeoutSeconds: 7200 }).text,
    `\u{1F6E1}\u{FE0F} <@${UID}>, your message was removed and you have been timed out for 2 hours: **Bad Words**.`);
  const kick = t({ status: 'applied', punishment: 'KICK' });
  assert.equal(kick.mention, false);
  assert.equal(kick.text, '\u{1F6E1}\u{FE0F} **someone** was kicked by automod: **Bad Words**.');
  assert.match(t({ status: 'applied', punishment: 'BAN' }).text, /was banned by automod/);
  const warned = t({ deleted: false, status: 'applied', punishment: 'WARN' });
  assert.equal(warned.text, `⚠\u{FE0F} <@${UID}>, you have been warned: **Bad Words**.`);
  assert.equal(warned.mention, true);
  assert.ok(t({ status: 'applied', punishment: 'TIMEOUT', timeoutSeconds: 2_419_200 }).text.length <= 200);
  assert.ok(buildNoticeText({ deleted: true, status: 'applied', punishment: 'BAN', author: { id: UID, username: 'x'.repeat(200) }, label: 'Excessive Mentions' }).text.length <= 200);
});

test('pipeline: hook-context details (no ctx.state, no stop) do not break a delete', async () => {
  const env = makeEnv();
  delete env.ctx.state;
  delete env.ctx.stop;
  await assert.doesNotReject(runAutomod(env.ctx));
  assert.equal(env.deletes.length, 1);
});

// ── Autocomplete and problems (pure paths) ───────────────────────────────────

function fakeAutocomplete(name, value, guildId = SNOW(990)) {
  const out = { choices: null };
  return {
    out,
    interaction: {
      guildId,
      responded: false,
      createdTimestamp: Date.now(),
      options: { getFocused: () => ({ name, value }) },
      respond: async (choices) => { out.choices = choices; },
    },
  };
}

test('autocomplete: rules answer synchronously, other fields answer [] when settings are unavailable', async () => {
  const rules = fakeAutocomplete('rule', 'ca');
  await automod.autocomplete(rules.interaction);
  assert.deepEqual(rules.out.choices.map((c) => c.value), ['caps']);
  assert.ok(rules.out.choices[0].name.length <= 100);
  const all = fakeAutocomplete('rule', '');
  await automod.autocomplete(all.interaction);
  assert.equal(all.out.choices.length, 10);
  const byLabel = fakeAutocomplete('rule', 'excessive');
  await automod.autocomplete(byLabel.interaction);
  assert.deepEqual(byLabel.out.choices.map((c) => c.value), ['caps', 'emojis', 'spoilers', 'mentions']);
  const broken = fakeAutocomplete('word', 'x');
  broken.interaction.options.getFocused = () => { throw new Error('boom'); };
  await assert.doesNotReject(automod.autocomplete(broken.interaction));
  assert.deepEqual(broken.out.choices, []);
});

test('collectProblems lists what would stop automod from working', () => {
  const guild = (perms) => ({ members: { me: { permissions: { has: (p) => perms.includes(p) } } } });
  const config = normalizeConfig({ version: 2, rules: {
    badWords: { action: 'ban' }, spam: { action: 'timeout' }, caps: { action: 'kick', roleMode: 'only' },
  } });
  const problems = automod.collectProblems(guild([]), { automodEnabled: true, modLogChannelId: null }, config);
  const text = problems.join('\n');
  assert.match(text, /Manage Messages/);
  assert.match(text, /Timeout Members/);
  assert.match(text, /Kick Members/);
  assert.match(text, /Ban Members/);
  assert.match(text, /Redis is unavailable/);
  assert.match(text, /Bad Words\*\* has no words or patterns/);
  assert.match(text, /Excessive Caps\*\* applies to nobody: its roles mode/);
  assert.match(text, /No mod-log channel/);
  const none = automod.collectProblems(guild([]), { automodEnabled: true, modLogChannelId: LOGCH }, normalizeConfig(null));
  assert.deepEqual(none, ['No rules are active. Use `/automod rule` to turn one on.']);
  const only = automod.collectProblems(guild([]), { automodEnabled: false }, config, { only: 'caps' });
  assert.ok(!only.some((p) => /mod-log|No rules/.test(p)));
});

// ── DB cases: the /automod command against a real settings row ───────────────

function fakeCommand({ guildId, group = null, sub, options = {}, guild } = {}) {
  const replies = [];
  const state = { deferred: null };
  const pick = (kind) => (name) => (Object.hasOwn(options, name) ? options[name] : null);
  const interaction = {
    guildId,
    guild: guild ?? { id: guildId, members: { me: { permissions: { has: () => true } } } },
    createdTimestamp: Date.now(),
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: (name, required) => {
        const v = Object.hasOwn(options, name) ? options[name] : null;
        if (required && v === null) throw new Error(`missing ${name}`);
        return v;
      },
      getInteger: pick('int'),
      getRole: pick('role'),
      getChannel: pick('channel'),
    },
    deferReply: async (o) => { state.deferred = o ?? {}; },
    editReply: async (p) => { replies.push(p); return {}; },
  };
  return { interaction, replies, state };
}

async function run(args) {
  const c = fakeCommand(args);
  await automod.execute(c.interaction);
  return { ...c, embed: c.replies.at(-1)?.embeds?.[0]?.toJSON?.() };
}

const dbTest = (name, fn) => test(name, { skip: skipDb }, fn);

dbTest('command: rules, words, patterns, domains, exemptions and enable/disable round-trip through the settings row', async () => {
  const guildId = SNOW(5006);
  const cmd = (over) => run({ guildId, ...over });
  const cfg = async () => (await getSettings(guildId)).automodConfig;
  try {
    await prisma.guildSettings.deleteMany({ where: { guildId } });

    // status on a fresh guild: everything off.
    let r = await cmd({ sub: 'status' });
    assert.equal(r.state.deferred.ephemeral, undefined, 'status is public');
    assert.match(r.embed.title, /Auto-Moderation/);
    assert.match(r.embed.description, /Status:\*\* ❌ Off/);
    assert.match(r.embed.fields[0].value, /Bad Words\*\* — Off/);
    assert.ok(r.embed.fields.some((f) => f.name === 'Lists'));

    // enable with no active rules.
    r = await cmd({ sub: 'enable' });
    assert.match(r.embed.title, /Automod Enabled/);
    assert.match(r.embed.description, /No rules are active yet/);
    assert.equal((await getSettings(guildId)).automodEnabled, true);

    // rule: validation errors.
    r = await cmd({ sub: 'rule', options: { rule: 'nope', action: 'delete' } });
    assert.match(r.embed.title, /Unknown Rule/);
    r = await cmd({ sub: 'rule', options: { rule: 'badWords', action: 'delete', threshold: 5 } });
    assert.match(r.embed.description, /has no threshold/);
    r = await cmd({ sub: 'rule', options: { rule: 'caps', action: 'delete', threshold: 20 } });
    assert.match(r.embed.description, /between 50 and 100/);
    r = await cmd({ sub: 'rule', options: { rule: 'caps', action: 'delete', timeout: '10m' } });
    assert.match(r.embed.description, /only applies to the \*\*Delete & timeout\*\* action/);
    r = await cmd({ sub: 'rule', options: { rule: 'caps', action: 'timeout', timeout: '40d' } });
    assert.match(r.embed.description, /between 1 minute and 28 days/);
    r = await cmd({ sub: 'rule', options: { rule: 'caps', action: 'timeout', timeout: 'soon' } });
    assert.match(r.embed.description, /between 1 minute and 28 days/);
    assert.equal((await cfg()).rules.caps.action, 'off', 'nothing was written by the failed attempts');

    // rule: success paths.
    r = await cmd({ sub: 'rule', options: { rule: 'Excessive Caps', action: 'timeout', threshold: 80, timeout: '2h', roles: 'only', channels: 'except' } });
    assert.match(r.embed.title, /Rule Updated/);
    assert.match(r.embed.description, /Delete & timeout · ≥ 80% capitals · 2 hours/);
    assert.match(r.embed.description, /applies to nobody/, 'only + empty list is flagged');
    let c = await cfg();
    assert.equal(c.version, 2);
    assert.equal(c.rules.caps.timeoutSeconds, 7200);
    assert.equal(c.rules.caps.threshold, 80);
    assert.equal(c.rules.caps.roleMode, 'only');
    r = await cmd({ sub: 'rule', options: { rule: 'caps', action: 'off' } });
    assert.match(r.embed.description, /is now off/);
    r = await cmd({ sub: 'rule', options: { rule: 'badWords', action: 'delete_warn' } });
    assert.match(r.embed.description, /no words or patterns yet/);

    // words (ephemeral).
    r = await cmd({ group: 'words', sub: 'add', options: { words: 'Ass, cunt,a, ass,!!,bad phrase' } });
    assert.equal(r.state.deferred.ephemeral, true);
    assert.match(r.embed.title, /Words Added/);
    assert.match(r.embed.description, /Added \*\*3\*\*: \|\|ass\|\|, \|\|cunt\|\|, \|\|bad phrase\|\|/);
    assert.match(r.embed.description, /ass \(already listed\)/);
    assert.match(r.embed.description, /a \(too short\)/);
    assert.match(r.embed.description, /!! \(no letters or digits\)/);
    assert.deepEqual((await cfg()).badWords, ['ass', 'cunt', 'bad phrase']);
    r = await cmd({ group: 'words', sub: 'add', options: { words: 'ass' } });
    assert.match(r.embed.title, /Nothing Added/);
    r = await cmd({ group: 'words', sub: 'list' });
    assert.match(r.embed.title, /Bad Words \(3\)/);
    assert.match(r.embed.description, /\|\|ass\|\|, \|\|cunt\|\|, \|\|bad phrase\|\|/);
    r = await cmd({ group: 'words', sub: 'remove', options: { word: 'CUNT' } });
    assert.match(r.embed.title, /Word Removed/);
    r = await cmd({ group: 'words', sub: 'remove', options: { word: 'zzz' } });
    assert.match(r.embed.title, /Not Listed/);

    // patterns (ephemeral).
    r = await cmd({ group: 'patterns', sub: 'add', options: { pattern: '(a+)+$' } });
    assert.match(r.embed.title, /Invalid Pattern/);
    r = await cmd({ group: 'patterns', sub: 'add', options: { pattern: 'a*' } });
    assert.match(r.embed.description, /matches empty text/);
    r = await cmd({ group: 'patterns', sub: 'add', options: { pattern: 'fr[e3]{2}\\s*n[i1]tro' } });
    assert.equal(r.state.deferred.ephemeral, true);
    assert.match(r.embed.title, /Pattern Added/);
    assert.match(r.embed.description, /Pattern \*\*#1\*\*/);
    r = await cmd({ group: 'patterns', sub: 'add', options: { pattern: 'fr[e3]{2}\\s*n[i1]tro' } });
    assert.match(r.embed.title, /Already Listed/);
    r = await cmd({ group: 'patterns', sub: 'list' });
    assert.match(r.embed.description, /\*\*#1\*\* `fr\[e3\]\{2\}/);
    r = await cmd({ group: 'patterns', sub: 'remove', options: { pattern: '1' } });
    assert.match(r.embed.title, /Pattern Removed/);
    r = await cmd({ group: 'patterns', sub: 'remove', options: { pattern: '1' } });
    assert.match(r.embed.title, /Not Listed/);
    assert.deepEqual((await cfg()).badWordPatterns, []);

    // domains (public).
    r = await cmd({ group: 'domains', sub: 'add', options: { domain: 'https://WWW.Evil.com/x' } });
    assert.equal(r.state.deferred.ephemeral, undefined);
    assert.match(r.embed.description, /\*\*evil\.com\*\* \(and its subdomains\) is now \*\*blocked\*\*/);
    assert.match(r.embed.description, /only blocked domains are removed/);
    assert.match(r.embed.description, /External Links\*\* rule is off/);
    r = await cmd({ group: 'domains', sub: 'add', options: { domain: 'evil.com', list: 'allowed' } });
    assert.match(r.embed.description, /removed from the blocked list/);
    r = await cmd({ group: 'domains', sub: 'add', options: { domain: 'evil.com', list: 'allowed' } });
    assert.match(r.embed.title, /Already Listed/);
    r = await cmd({ group: 'domains', sub: 'add', options: { domain: 'not a domain' } });
    assert.match(r.embed.title, /Invalid Domain/);
    r = await cmd({ group: 'domains', sub: 'list' });
    assert.ok(r.embed.fields.find((f) => f.name === 'Allowed').value.includes('evil.com'));
    assert.match(r.embed.description, /every link is blocked except allowed domains/);
    r = await cmd({ group: 'domains', sub: 'remove', options: { domain: 'evil.com' } });
    assert.match(r.embed.title, /Domain Removed/);
    r = await cmd({ group: 'domains', sub: 'remove', options: { domain: 'evil.com' } });
    assert.match(r.embed.title, /Not Listed/);

    // exemptions (public).
    const role = { id: SNOW(5101) };
    const channel = { id: SNOW(5102), type: 4 };
    r = await cmd({ group: 'exempt', sub: 'add', options: {} });
    assert.match(r.embed.title, /Nothing Selected/);
    r = await cmd({ group: 'exempt', sub: 'add', options: { role: { id: guildId } } });
    assert.match(r.embed.title, /Invalid Role/);
    r = await cmd({ group: 'exempt', sub: 'add', options: { role, channel } });
    assert.match(r.embed.title, /Exemptions Updated/);
    assert.match(r.embed.description, /Every channel and thread inside this category is covered/);
    c = await cfg();
    assert.deepEqual(c.exemptRoleIds, [role.id]);
    assert.deepEqual(c.exemptChannelIds, [channel.id]);
    r = await cmd({ group: 'exempt', sub: 'add', options: { role } });
    assert.match(r.embed.title, /No Change/);
    r = await cmd({ group: 'exempt', sub: 'add', options: { role, rule: 'links' } });
    assert.match(r.embed.description, /External Links\*\* ignores/);
    assert.deepEqual((await cfg()).rules.links.roleIds, [role.id]);
    r = await cmd({ group: 'exempt', sub: 'add', options: { role, rule: 'nope' } });
    assert.match(r.embed.title, /Unknown Rule/);
    r = await cmd({ group: 'exempt', sub: 'remove', options: { role, channel } });
    assert.match(r.embed.title, /Exemptions Updated/);
    assert.deepEqual((await cfg()).exemptRoleIds, []);
    r = await cmd({ group: 'exempt', sub: 'remove', options: { role } });
    assert.match(r.embed.title, /No Change/);

    // status now shows the configuration; enable lists active rules; disable keeps them.
    r = await cmd({ sub: 'status' });
    assert.match(r.embed.description, /Status:\*\* ✅ On/);
    assert.match(r.embed.fields[0].value, /Bad Words\*\* — Delete & warn · 2 words, 0 patterns/);
    r = await cmd({ sub: 'enable' });
    assert.match(r.embed.description, /Active rules:/);
    assert.match(r.embed.description, /Bad Words/);
    r = await cmd({ sub: 'disable' });
    assert.match(r.embed.title, /Automod Disabled/);
    assert.equal((await getSettings(guildId)).automodEnabled, false);
    assert.equal((await cfg()).rules.badWords.action, 'delete_warn', 'disable keeps the rules');
  } finally {
    await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  }
});

dbTest('command: a legacy v1 config is shown as such and upgraded by the next write', async () => {
  const guildId = SNOW(5007);
  try {
    await prisma.guildSettings.deleteMany({ where: { guildId } });
    await updateSettings(guildId, { automodEnabled: true, automodConfig: { filterInvites: true, automodAction: 'BAN', badWords: ['Foo'], filterBadWords: true } });
    let r = await run({ guildId, sub: 'status' });
    assert.match(r.embed.footer.text, /old format/);
    assert.match(r.embed.fields[0].value, /Server Invites\*\* — Delete & ban/);
    assert.match(r.embed.fields[0].value, /Bad Words\*\* — Delete & ban · 1 words/);
    r = await run({ guildId, group: 'words', sub: 'add', options: { words: 'bar' } });
    const stored = (await getSettings(guildId)).automodConfig;
    assert.equal(stored.version, 2);
    assert.equal(stored.rules.invites.action, 'ban');
    assert.deepEqual(stored.badWords, ['foo', 'bar']);
    r = await run({ guildId, sub: 'status' });
    assert.equal(r.embed.footer, undefined, 'no legacy footer after the upgrade');
  } finally {
    await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  }
});

dbTest('command: concurrent word additions do not lose updates', async () => {
  const guildId = SNOW(5008);
  try {
    await prisma.guildSettings.deleteMany({ where: { guildId } });
    await Promise.all(['alpha', 'bravo', 'charlie', 'delta'].map((w) => run({ guildId, group: 'words', sub: 'add', options: { words: w } })));
    assert.deepEqual([...(await getSettings(guildId)).automodConfig.badWords].sort(), ['alpha', 'bravo', 'charlie', 'delta']);
  } finally {
    await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  }
});

dbTest('command: a database failure is reported, not thrown', async () => {
  const guildId = SNOW(5009);
  const real = prisma.$transaction;
  prisma.$transaction = async () => { throw new Error('db down'); };
  try {
    const r = await run({ guildId, sub: 'rule', options: { rule: 'caps', action: 'delete' } });
    assert.match(r.embed.title, /Could Not Save/);
  } finally {
    prisma.$transaction = real;
  }
});

dbTest('command: /automod status stays within the 6000-character embed limit with every list full', async () => {
  const guildId = SNOW(5010);
  const ids = (base, n) => Array.from({ length: n }, (_, i) => SNOW(base + i));
  try {
    await prisma.guildSettings.deleteMany({ where: { guildId } });
    const rules = Object.fromEntries(RULE_KEYS.map((k) => [k, {
      action: 'ban', roleMode: 'only', roleIds: ids(6000, 50), channelMode: 'only', channelIds: ids(7000, 50),
    }]));
    await updateSettings(guildId, {
      automodEnabled: true,
      modLogChannelId: SNOW(6),
      automodConfig: {
        version: 2,
        rules,
        badWords: Array.from({ length: 500 }, (_, i) => `word${i}`),
        blockedDomains: Array.from({ length: 200 }, (_, i) => `blocked${i}.com`),
        exemptRoleIds: ids(8000, 50),
        exemptChannelIds: ids(9000, 50),
      },
    });
    const r = await run({ guildId, sub: 'status' });
    const e = r.embed;
    const total = (e.title?.length ?? 0) + (e.description?.length ?? 0) + (e.footer?.text.length ?? 0)
      + e.fields.reduce((n, f) => n + f.name.length + f.value.length, 0);
    assert.ok(total <= 6000, `embed has ${total} characters`);
    assert.ok(e.fields.length <= 25);
    for (const f of e.fields) assert.ok(f.value.length >= 1 && f.value.length <= 1024, f.name);
    assert.ok(e.fields.some((f) => f.name === 'Rules'));
  } finally {
    await prisma.guildSettings.deleteMany({ where: { guildId } }).catch(() => {});
  }
});
