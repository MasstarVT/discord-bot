// Regression tests from the moderation-area review (moderation, automod, captcha, logging). Offline: no Discord,
// Postgres or Redis. Prisma delegates are replaced with in-memory stubs for the captcha case.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back and Postgres is never reached (the delegates below are stubs).
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';
delete process.env.BOT_OWNER_ID;

const { Collection, PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const automod = await import('../src/services/automodService.js');
const captcha = await import('../src/services/captchaService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { getSettingsSafe } = await import('../src/services/settingsService.js');

const { normalizeConfig, buildMatchers, evaluateContentRules, extractLinkHosts, textForLinks, findInvites } = automod;
const { runTimeoutJob, handleCaptchaButton } = captcha;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const realGuildSettings = prisma.guildSettings;
const realVerification = prisma.captchaVerification;
const realJobs = prisma.scheduledJob;
after(async () => {
  prisma.guildSettings = realGuildSettings;
  prisma.captchaVerification = realVerification;
  prisma.scheduledJob = realJobs;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── Automod: links and invites wrapped in markdown ───────────────────────────

function linkEnv(over = {}) {
  const config = normalizeConfig({ version: 2, rules: { links: { action: 'delete' }, invites: { action: 'delete' } }, ...over });
  return { config, matchers: buildMatchers(config) };
}
const hits = (env, content, rule) => evaluateContentRules({ content }, [rule], env.config, env.matchers, null).length;

test('links: markdown marks around a link do not hide or change its host', () => {
  const block = linkEnv({ blockedDomains: ['evil.com'] });
  for (const wrapped of [
    '**https://evil.com**', '~~https://evil.com~~', '*https://evil.com*', '_https://evil.com_', '__https://evil.com__',
    '||https://evil.com||', '**www.evil.com**', '_www.evil.com_', 'hi **https://evil.com** there',
  ]) {
    assert.equal(hits(block, wrapped, 'links'), 1, `blocked domain must match: ${wrapped}`);
  }
  assert.equal(hits(block, '**https://good.org**', 'links'), 0);
  assert.equal(hits(block, 'https://x_y.evil.com', 'links'), 1, 'a subdomain of a blocked domain still matches');

  // Allowlist mode (no blocked domain): an allowed domain stays allowed when wrapped, others are caught.
  const allow = linkEnv({});
  for (const ok of ['**https://tenor.com**', '__https://tenor.com__', '~~https://giphy.com~~']) {
    assert.equal(hits(allow, ok, 'links'), 0, `allowed domain must not match: ${ok}`);
  }
  assert.equal(hits(allow, '**https://evil.com**', 'links'), 1);
  assert.equal(hits(allow, '_https://evil.com_', 'links'), 1);
});

test('extractLinkHosts: trailing markdown marks are cut, paths are untouched', () => {
  assert.deepEqual(extractLinkHosts('**https://evil.com**'), ['evil.com']);
  assert.deepEqual(extractLinkHosts('_https://evil.com/a_b_'), ['evil.com']);
  assert.deepEqual(extractLinkHosts('foo (https://evil.com/x).'), ['evil.com']);
});

test('invites: an invite in markdown italics or underline is still an invite', () => {
  const env = linkEnv({});
  for (const wrapped of ['_discord.gg/abc_', '__discord.gg/abc__', '**discord.gg/abc**', '~~discord.gg/abc~~', '_https://discord.gg/abc_']) {
    assert.equal(hits(env, wrapped, 'invites'), 1, wrapped);
  }
  assert.deepEqual(findInvites(textForLinks('mydiscord.gg/abc')), [], 'a longer hostname is still not an invite');
  assert.deepEqual(findInvites('discord.game'), []);
});

// ── Captcha: the timeout job and a Verify that is already running ────────────

const perms = (...keys) => new PermissionsBitField(keys.map((k) => PermissionFlagsBits[k]));
const GID = '955030000000000001';
const UID = '955030111110000001';
const ROLE = '955030222220000001';

function mkGuild() {
  const everyone = { id: GID, position: 0, managed: false, permissions: perms('ViewChannel', 'SendMessages'), name: 'everyone' };
  const role = { id: ROLE, position: 1, managed: false, permissions: perms(), name: 'verified' };
  const me = {
    id: 'B', permissions: perms('ManageRoles', 'KickMembers'),
    roles: { highest: { position: 10 }, cache: new Collection() },
  };
  const guild = {
    id: GID, name: 'Test', ownerId: 'O', memberCount: 3, available: true,
    roles: { everyone, cache: new Collection([[GID, everyone], [ROLE, role]]) },
    members: { me, cache: new Collection(), fetch: async ({ user }) => guild.members.cache.get(user) },
    channels: { cache: new Collection() },
    client: { user: { id: 'B', tag: 'bot#0' } },
  };
  me.guild = guild;
  return guild;
}

test('captcha.timeout waits for a Verify in flight: a member who passes at the deadline is not kicked', async () => {
  const guild = mkGuild();
  const settings = {
    guildId: GID, captchaEnabled: true, captchaMode: 'BUTTON', captchaRoleId: ROLE, captchaTimeoutMinutes: 30,
    modRoleIds: [], immuneRoleIds: [], autoroleIds: [], muteRoleId: null, embedColor: 0x123456,
  };
  prisma.guildSettings = { findUnique: async () => ({ ...settings }), upsert: async () => ({ ...settings }) };
  await getSettingsSafe(GID); // warm the memo
  let rowExists = true;
  prisma.captchaVerification = {
    findUnique: async () => (rowExists ? { guildId: GID, userId: UID, answerHash: null, attempts: 0, expiresAt: null } : null),
    deleteMany: async () => { const count = rowExists ? 1 : 0; rowExists = false; return { count }; },
  };
  prisma.scheduledJob = { deleteMany: async () => ({ count: 0 }) };

  const member = {
    id: UID, guild, pending: false, kickable: true, permissions: perms(), joinedTimestamp: Date.now() - 3_600_000,
    user: { id: UID, bot: false, tag: 'u#0' },
    roles: { cache: new Collection(), highest: { position: 1 } },
    dms: [],
  };
  member.send = async (payload) => { member.dms.push(payload); };
  guild.members.cache.set(UID, member);

  // The Verify press: the role add is slow, so the member is still unverified when the deadline job starts.
  let release;
  const gate = new Promise((r) => { release = r; });
  let addStarted;
  const started = new Promise((r) => { addStarted = r; });
  member.roles.add = async (rid) => {
    addStarted();
    await gate;
    member.roles.cache.set(rid, guild.roles.cache.get(rid));
    return member;
  };
  const replies = [];
  const interaction = {
    customId: 'captcha:start', guild, guildId: GID, member, user: member.user, createdTimestamp: Date.now(),
    message: { flags: { has: () => false } },
    inCachedGuild: () => true,
    deferReply: async () => {},
    deferUpdate: async () => {},
    editReply: async (p) => { replies.push(p); return {}; },
    reply: async (p) => { replies.push(p); return {}; },
  };
  const verifying = handleCaptchaButton(interaction);
  await started;

  const kicks = [];
  const mod = async (input) => { kicks.push(input); return { ok: true }; };
  const job = runTimeoutJob({ payload: { userId: UID } }, { guild, attempt: 1 }, { executeModAction: mod });
  await sleep(100); // long enough for an unserialised job to read the row, DM and kick
  assert.equal(kicks.length, 0, 'the job must not kick while the member is being verified');

  release();
  await verifying;
  await job;
  assert.equal(kicks.length, 0, 'a verified member is never kicked');
  assert.equal(member.dms.length, 0, 'and is not told they were removed');
  assert.equal(member.roles.cache.has(ROLE), true);
  assert.equal(rowExists, false);
  assert.ok(replies.length > 0);
});

// ── Automod: bad words and Discord underscores, scripts without spaces ───────

function wordEnv(badWords) {
  const config = normalizeConfig({ version: 2, rules: { badWords: { action: 'delete' } }, badWords });
  return { config, matchers: buildMatchers(config) };
}
const wordHit = (env, text) => automod.checkBadWords(text, env.config, env.matchers) !== null;

test('bad words: underscores around a word are Discord italics/underline, not word characters', () => {
  const env = wordEnv(['badword', 'ass']);
  for (const text of ['_badword_', '__badword__', '___badword___', '__badword', 'badword_', '(_badword_)', 'you _ass_!', 'x __badword__ y']) {
    assert.ok(wordHit(env, text), `should match: ${text}`);
  }
  assert.equal(automod.textForWords('__bad__'), ' bad ');
});

test('bad words: an underscore between word parts and an escaped underscore stay word characters', () => {
  const env = wordEnv(['badword', 'ass', 'cunt']);
  for (const text of ['snake_badword_case', 'snake_badword', 'badword_pls', 'foo__badword__bar', 'cunts_', 'my_ass_name', '\\_badword\\_']) {
    assert.ok(!wordHit(env, text), `should not match: ${text}`);
  }
  assert.equal(automod.textForWords('\\_'), '_');
  assert.equal(automod.textForWords('b**a**d ||x|| \\_'), 'bad x _');
  assert.equal(automod.textForWords('a\\_b_'), 'a_b ');
});

test('bad words: link handling is untouched (markdown marks still glue a link into one token)', () => {
  assert.equal(textForLinks('hi**discord.gg/abc**'), 'hi**discord.gg/abc**');
  assert.equal(textForLinks('discord\\.gg/abc'), 'discord.gg/abc');
});

test('bad words: entries in scripts without word spaces match inside running text', () => {
  const env = wordEnv(['坏话', 'バカ', 'ばか', 'คำหยาบ', 'ပြဿနာ', 'ពាក្យ', 'ຄຳ', 'bad word', '한글']);
  for (const text of ['你这个坏话说的', 'あのバカだ', 'あのばかだ', 'คุณคำหยาบมาก', 'မပြဿနာများ', 'ពាក្យអាក្រក់', 'ເປັນຄຳຫຍາບ', 'this is a BAD  word!']) {
    assert.ok(wordHit(env, text), `should match: ${text}`);
  }
  // Hangul is written with spaces: it stays a whole-word match; a mixed entry keeps whole-word matching.
  assert.ok(wordHit(env, '이 한글 입니다'));
  assert.ok(!wordHit(env, '한글입니다'));
  const mixed = wordEnv(['abc坏']);
  assert.ok(wordHit(mixed, 'x abc坏 y'));
  assert.ok(!wordHit(mixed, 'xabc坏y'));
  assert.ok(!wordHit(wordEnv(['ass']), '坏话 passing'));
  assert.equal(automod.isSpacelessWord('坏话'), true);
  assert.equal(automod.isSpacelessWord('坏话x'), false);
  assert.equal(automod.isSpacelessWord('한글'), false);
  assert.equal(automod.isSpacelessWord(''), false);
});
