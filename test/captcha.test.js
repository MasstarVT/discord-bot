// pkg-captcha tests (pkg-captcha.md §8): the pure helpers, the captcha image, payloads, the plugin and command
// declarations, plus the I/O paths against fakes. Offline by default (Redis and Postgres unreachable). With
// TEST_DATABASE_URL (a migrated database) the DB cases also run:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/captcha.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back (the image and DM caps fail open), and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { Collection, PermissionFlagsBits, PermissionsBitField, ButtonStyle } = await import('discord.js');
const svc = await import('../src/services/captchaService.js');
const cmd = await import('../src/commands/admin/captcha.js');
const { plugin } = await import('../src/plugins/captcha.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { updateSettings, peekSettings } = await import('../src/services/settingsService.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');

const {
  CAPTCHA_ALPHABET, CODE_LENGTH, CUSTOM_IDS, MAX_ATTEMPTS, MSG, VERIFY_EXISTING_BATCH,
  generateCode, normalizeAnswer, hashAnswer, answerMatches, evaluateAnswer, validateTimeoutMinutes, timeoutRunAt,
  parseVerifyButton, panelTemplate, buildPanelPayload, buildChallengePayload, isRoleNewlyAdded, isLocked, everyoneCanView,
  botSeesWithoutEveryone, lockPlan, isKickExempt, checkVerifiedRole, progressText, discordErrorText, renderCaptchaImage,
  isImageCaptchaAvailable, applyLock, removeLock, publishPanel, refreshPanel, diagnoseCaptcha, timeoutJobKey,
  onMemberJoin, onMemberLeave, onMemberUpdate, runTimeoutJob, handleCaptchaButton, handleCaptchaModal,
  completeVerification, issueChallenge, startVerifyExisting, getBulkRun, isBulkTarget, CaptchaConfigError,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const GID = (n) => `95502900000${String(n).padStart(7, '0')}`; // 18 digits, unique to this package
const UID = (n) => `95502911111${String(n).padStart(7, '0')}`;
const DB_GUILDS = [];
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // Leave nothing behind in a shared test database (CaptchaVerification rows cascade with the settings row).
    await prisma.scheduledJob.deleteMany({ where: { guildId: { startsWith: '95502900000' } } }).catch(() => {});
    await prisma.guildSettings.deleteMany({ where: { guildId: { startsWith: '95502900000' } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ────────────────────────────────────────────────────────────────────

const perms = (...keys) => new PermissionsBitField(keys.map((k) => PermissionFlagsBits[k]));
const apiError = (code, message = 'api error') => Object.assign(new Error(message), { code });

function mkRole(id, { position = 1, managed = false, permissions = perms() } = {}) {
  return { id, position, managed, permissions, name: `role-${id}` };
}

/** A guild with @everyone (id = guild id) and a bot member `B` holding ManageRoles and KickMembers. */
function mkGuild({ id = 'G', name = 'Test', everyonePerms = perms('ViewChannel', 'SendMessages'), botRoles = [] } = {}) {
  const everyone = mkRole(id, { permissions: everyonePerms });
  const roles = new Collection([[id, everyone]]);
  const me = {
    id: 'B', permissions: perms('ManageRoles', 'KickMembers'),
    roles: { highest: { position: 10 }, cache: new Collection(botRoles.map((r) => [r.id, r])) },
  };
  const guild = {
    id, name, ownerId: 'O', memberCount: 3, available: true,
    roles: { everyone, cache: roles },
    members: { me, cache: new Collection(), fetch: async () => guild.members.cache },
    channels: { cache: new Collection() },
    client: { user: { id: 'B', tag: 'bot#0' } },
  };
  me.guild = guild;
  return guild;
}

function addRole(guild, id, opts) {
  const r = mkRole(id, opts);
  guild.roles.cache.set(id, r);
  return r;
}

function mkMember(guild, id, { roleIds = [], pending = false, bot = false, permissions = perms(), kickable = true, joined = Date.now() } = {}) {
  const m = {
    id, guild, pending, kickable, permissions, joinedTimestamp: joined,
    user: { id, bot, tag: `u${id}#0` },
    roles: { cache: new Collection(), highest: { position: 1 }, calls: [] },
    dms: [],
  };
  for (const rid of roleIds) m.roles.cache.set(rid, guild.roles.cache.get(rid) ?? { id: rid });
  m.roles.add = async (rid, reason) => {
    m.roles.calls.push({ id: rid, reason });
    m.roles.cache.set(rid, guild.roles.cache.get(rid) ?? { id: rid });
    return m;
  };
  m.send = async (payload) => { m.dms.push(payload); };
  guild.members.cache.set(id, m);
  return m;
}

function mkChannel(guild, id, { missing = [], messages = new Map(), sendError = null } = {}) {
  const sent = [];
  const channel = {
    id, guild, name: 'verification', sent, messages: { store: messages },
    permissionOverwrites: {
      cache: new Collection(), edited: [],
      edit: async (target, o, opts) => {
        channel.permissionOverwrites.edited.push({ target, o, opts });
        channel.permissionOverwrites.cache.set(target.id, { allow: perms(...Object.keys(o).filter((k) => o[k])) });
      },
    },
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: (bit) => !missing.some((k) => PermissionFlagsBits[k] === bit) }),
    send: async (payload) => {
      if (sendError) throw sendError;
      const msg = { id: `95502933333${String(sent.length + 1).padStart(7, '0')}`, url: 'https://discord.com/channels/x/y/z', edits: [], edit: async (p) => { msg.edits.push(p); } };
      sent.push(payload);
      messages.set(msg.id, msg);
      return msg;
    },
  };
  channel.messages.fetch = async (mid) => {
    const m = messages.get(mid);
    if (!m) throw apiError(10008);
    return m;
  };
  channel.messages.delete = async (mid) => { channel.deleted = [...(channel.deleted ?? []), mid]; messages.delete(mid); };
  guild.channels.cache.set(id, channel);
  return channel;
}

/** A component or modal interaction that records what happened. */
function mkInteraction(guild, member, { customId = 'captcha:start', fromEphemeral = false, fromMessage = true, input = '', createdTimestamp = Date.now() } = {}) {
  const log = { defers: [], edits: [], replies: [], modals: [], followUps: [] };
  return {
    customId, guild, guildId: guild.id, member, user: member.user, createdTimestamp, log,
    message: { flags: { has: () => fromEphemeral } },
    inCachedGuild: () => true,
    isFromMessage: () => fromMessage,
    fields: { getTextInputValue: () => input },
    deferReply: async (o) => { log.defers.push(['reply', o]); },
    deferUpdate: async () => { log.defers.push(['update']); },
    editReply: async (p) => { log.edits.push(p); return {}; },
    followUp: async (p) => { log.followUps.push(p); return {}; },
    reply: async (p) => { log.replies.push(p); return {}; },
    showModal: async (m) => { log.modals.push(m); },
  };
}

/** A slash-command interaction with an options bag. */
function mkSlash(guild, member, sub, o = {}) {
  const log = { defers: [], edits: [], replies: [], modals: [], followUps: [], deleted: 0 };
  const pick = (n) => (Object.hasOwn(o, n) ? o[n] : null);
  return {
    guild, guildId: guild.id, member, user: member.user, createdTimestamp: Date.now(), log, deferred: false, replied: false,
    options: {
      getSubcommand: () => sub, getSubcommandGroup: () => null, get: (n) => pick(n),
      getString: (n) => pick(n), getBoolean: (n) => pick(n), getInteger: (n) => pick(n),
      getRole: (n) => pick(n), getChannel: (n) => pick(n),
    },
    inCachedGuild: () => true,
    deferReply: async function deferReply() { log.defers.push(['reply']); this.deferred = true; this.ephemeral = false; },
    editReply: async (p) => { log.edits.push(p); return {}; },
    followUp: async (p) => { log.followUps.push(p); return {}; },
    deleteReply: async () => { log.deleted += 1; },
    reply: async function reply(p) { log.replies.push(p); this.replied = true; return {}; },
    showModal: async (m) => { log.modals.push(m); },
  };
}

const lastEmbed = (list) => list.at(-1)?.embeds?.[0]?.toJSON?.() ?? null;
const embedText = (e) => `${e?.title ?? ''}\n${e?.description ?? ''}`;

// ── 1-6. Pure helpers ────────────────────────────────────────────────────────

test('generateCode: deterministic with a fake rand, always inside the alphabet by default', () => {
  assert.equal(generateCode(() => 0), 'AAAAAA');
  for (let i = 0; i < 1000; i++) assert.match(generateCode(), /^[ACEFHJKMNPRTWXY34679]{6}$/);
});

test('CAPTCHA_ALPHABET: 20 unique glyphs, no ambiguous characters, no lowercase', () => {
  assert.equal(CAPTCHA_ALPHABET.length, 20);
  assert.equal(new Set(CAPTCHA_ALPHABET).size, 20);
  for (const bad of '0OQD1IL2Z5S8BGUV') assert.ok(!CAPTCHA_ALPHABET.includes(bad), bad);
  assert.equal(CAPTCHA_ALPHABET, CAPTCHA_ALPHABET.toUpperCase());
  assert.equal(CODE_LENGTH, 6);
});

test('normalizeAnswer and hashAnswer', () => {
  assert.equal(normalizeAnswer(' 7kx-4pm '), '7KX4PM');
  assert.equal(normalizeAnswer(null), '');
  assert.equal(hashAnswer('G', 'U', '7kx4pm'), createHash('sha256').update('G:U:7KX4PM').digest('hex'));
  // The hash is bound to the guild and the user.
  assert.notEqual(hashAnswer('G', 'U', '7KX4PM'), hashAnswer('G', 'V', '7KX4PM'));
  assert.notEqual(hashAnswer('G', 'U', '7KX4PM'), hashAnswer('H', 'U', '7KX4PM'));
});

test('answerMatches: case and spacing free, wrong length and malformed hashes are false and never throw', () => {
  const h = hashAnswer('G', 'U', '7KX4PM');
  assert.equal(answerMatches(h, 'G', 'U', '7KX4PM'), true);
  assert.equal(answerMatches(h, 'G', 'U', ' 7kx 4pm '), true);
  assert.equal(answerMatches(h, 'G', 'U', '7kx-4pm'), true);
  assert.equal(answerMatches(h, 'G', 'U', '7KX4PN'), false);
  assert.equal(answerMatches(h, 'G', 'U', '7KX4P'), false);
  assert.equal(answerMatches(h, 'G', 'U', '7KX4PMM'), false);
  assert.equal(answerMatches(h, 'G', 'V', '7KX4PM'), false);
  for (const bad of [null, undefined, '', 'abc', 'z'.repeat(64), 'g'.repeat(64), 'a'.repeat(63), 'a'.repeat(65), 12345]) {
    assert.equal(answerMatches(bad, 'G', 'U', '7KX4PM'), false);
  }
});

test('evaluateAnswer matrix', () => {
  const now = 1_000_000;
  const h = hashAnswer('G', 'U', '7KX4PM');
  const live = (over = {}) => ({ answerHash: h, attempts: 0, expiresAt: new Date(now + 60_000), ...over });
  const ev = (row, input = '7KX4PM') => svc.evaluateAnswer({ row, guildId: 'G', userId: 'U', input, now });
  assert.deepEqual(ev(null), { result: 'none' });
  assert.deepEqual(ev(live({ answerHash: null })), { result: 'none' });
  assert.deepEqual(ev(live({ expiresAt: new Date(now - 1) })), { result: 'expired' });
  assert.deepEqual(ev(live({ expiresAt: null })), { result: 'expired' });
  assert.deepEqual(ev(live({ attempts: 3 })), { result: 'exhausted', attemptsLeft: 0 });
  assert.deepEqual(ev(live()), { result: 'ok' });
  assert.deepEqual(ev(live({ attempts: 2 })), { result: 'ok' });
  assert.deepEqual(ev(live(), 'AAAAAA'), { result: 'wrong', attemptsLeft: 2 });
  assert.deepEqual(ev(live({ attempts: 1 }), 'AAAAAA'), { result: 'wrong', attemptsLeft: 1 });
  assert.deepEqual(ev(live({ attempts: 2 }), 'AAAAAA'), { result: 'exhausted', attemptsLeft: 0 });
  assert.equal(MAX_ATTEMPTS, 3);
  assert.equal(typeof evaluateAnswer, 'function');
});

test('validateTimeoutMinutes and timeoutRunAt', () => {
  assert.deepEqual(validateTimeoutMinutes(0), { minutes: 0 });
  assert.ok(validateTimeoutMinutes(4).error);
  assert.deepEqual(validateTimeoutMinutes(5), { minutes: 5 });
  assert.deepEqual(validateTimeoutMinutes(10080), { minutes: 10080 });
  assert.ok(validateTimeoutMinutes(10081).error);
  assert.ok(validateTimeoutMinutes(2.5).error);
  assert.ok(validateTimeoutMinutes('5').error);
  const now = 5_000_000_000;
  assert.equal(timeoutRunAt(now - 1000, 10, now).getTime(), now - 1000 + 600_000);
  assert.equal(timeoutRunAt(now - 3_600_000, 10, now).getTime(), now + 60_000); // floor: never less than a minute away
  assert.equal(timeoutRunAt(null, 10, now).getTime(), now + 600_000);
});

test('parseVerifyButton: defaults, trimming, unknown styles (including inherited names) and clamping', () => {
  assert.deepEqual(parseVerifyButton(undefined), { label: 'Verify', style: 'SUCCESS' });
  assert.deepEqual(parseVerifyButton({ label: '  Go ', style: 'DANGER' }), { label: 'Go', style: 'DANGER' });
  assert.equal(parseVerifyButton({ style: 'LINK' }).style, 'SUCCESS');
  assert.equal(parseVerifyButton({ style: 'toString' }).style, 'SUCCESS'); // not an own key of the style table
  assert.equal(parseVerifyButton({ style: 'constructor' }).style, 'SUCCESS');
  assert.ok(parseVerifyButton({ label: 'x'.repeat(81) }).label.length <= 80);
  assert.equal(parseVerifyButton({ label: '   ' }).label, 'Verify');
});

test('panelTemplate: default, stored template without the verifyButton key, garbage falls back', () => {
  assert.equal(panelTemplate(null).embed.title, '🛡️ Verification required');
  const t = panelTemplate({ captchaTemplate: { embed: { title: 'X' }, verifyButton: { label: 'Go', style: 'DANGER' } } });
  assert.equal(t.embed.title, 'X');
  assert.ok(!('verifyButton' in t));
  assert.equal(panelTemplate({ captchaTemplate: { embed: 5 } }).embed.title, '🛡️ Verification required');
  assert.equal(panelTemplate({ captchaTemplate: 'nope' }).embed.title, '🛡️ Verification required');
});

test('buildPanelPayload: colour, one Verify button last, no pings, server token rendered, never blank', () => {
  const guild = mkGuild({ name: 'Test' });
  const settings = { embedColor: 0x123456, captchaTemplate: null };
  const p = buildPanelPayload(settings, guild, null);
  assert.equal(p.embeds[0].toJSON().color, 0x123456);
  assert.match(p.embeds[0].toJSON().description, /Welcome to \*\*Test\*\*/);
  assert.deepEqual(p.allowedMentions, { parse: [] });
  const row = p.components.at(-1).toJSON();
  assert.equal(row.components.length, 1);
  assert.equal(row.components[0].custom_id, 'captcha:start');
  assert.equal(row.components[0].label, 'Verify');
  assert.equal(row.components[0].style, ButtonStyle.Success);

  const q = buildPanelPayload({ embedColor: null, captchaTemplate: { embed: { title: 'Hi' }, verifyButton: { style: 'PRIMARY', label: 'Go' } } }, guild, null);
  const b = q.components.at(-1).toJSON().components[0];
  assert.equal(b.style, ButtonStyle.Primary);
  assert.equal(b.label, 'Go');

  // A colour-only template renders to nothing visible: the default text is shown instead.
  const c = buildPanelPayload({ captchaTemplate: { embed: { color: 0xff0000 } } }, guild, null);
  assert.ok(c.embeds.length === 1 || c.content);
});

test('buildChallengePayload: static ids, static attachment name, no alt text, no code anywhere in the text', async () => {
  const image = await renderCaptchaImage('7KX4PM');
  const p = buildChallengePayload({ settings: null, expiresAt: new Date(Date.now() + 600_000), image });
  const json = p.embeds[0].toJSON();
  assert.equal(json.image.url, 'attachment://captcha.png');
  assert.deepEqual(p.components[0].toJSON().components.map((c) => c.custom_id), ['captcha:answer', 'captcha:start']);
  assert.equal(p.files.length, 1);
  assert.equal(p.files[0].name, 'captcha.png');
  assert.equal(p.files[0].description ?? undefined, undefined); // nothing that becomes alt text
  assert.deepEqual(p.attachments, []);
  assert.doesNotMatch(JSON.stringify(json), /\b[ACEFHJKMNPRTWXY34679]{6}\b/);
  assert.equal(CUSTOM_IDS.answerField, 'captcha-answer');
});

test('isRoleNewlyAdded', () => {
  const m = (ids) => ({ roles: { cache: new Set(ids) } });
  assert.equal(isRoleNewlyAdded({ oldMember: m([]), newMember: m(['R']), oldKnown: true, roleId: 'R' }), true);
  assert.equal(isRoleNewlyAdded({ oldMember: m(['R']), newMember: m(['R']), oldKnown: true, roleId: 'R' }), false);
  assert.equal(isRoleNewlyAdded({ oldMember: m(['R']), newMember: m([]), oldKnown: true, roleId: 'R' }), false);
  assert.equal(isRoleNewlyAdded({ oldMember: m([]), newMember: m(['R']), oldKnown: false, roleId: 'R' }), true);
  assert.equal(isRoleNewlyAdded({ oldMember: m([]), newMember: m(['R']), oldKnown: true, roleId: null }), false);
});

test('isLocked, everyoneCanView, botSeesWithoutEveryone and lockPlan', () => {
  const open = mkGuild();
  assert.equal(isLocked(open), false);
  const locked = mkGuild({ everyonePerms: perms('SendMessages') });
  assert.equal(isLocked(locked), true);
  // Administrator on @everyone must not hide a lock (its View Channels flag is off), or `lock off` could never undo it.
  assert.equal(isLocked(mkGuild({ everyonePerms: perms('Administrator', 'SendMessages') })), true);
  assert.equal(isLocked(mkGuild({ everyonePerms: perms('Administrator', 'ViewChannel') })), false);

  // The bot's only View Channels comes from @everyone: it would be hidden by the lock.
  assert.equal(botSeesWithoutEveryone(open), false);
  const withRole = mkGuild({ botRoles: [mkRole('BR', { permissions: perms('ViewChannel') })] });
  assert.equal(botSeesWithoutEveryone(withRole), true);
  const admin = mkGuild({ botRoles: [mkRole('BA', { permissions: perms('Administrator') })] });
  assert.equal(botSeesWithoutEveryone(admin), true);

  const channel = mkChannel(open, 'C1');
  assert.equal(everyoneCanView(channel), true);
  const hidden = mkChannel(open, 'C2', { missing: ['ViewChannel'] });
  assert.equal(everyoneCanView(hidden), false);

  const role = addRole(open, 'RV');
  assert.deepEqual(lockPlan(open, role, channel), { channel: true, role: true, everyone: true });
  channel.permissionOverwrites.cache.set(open.id, { allow: perms('ViewChannel') });
  role.permissions = perms('ViewChannel');
  assert.deepEqual(lockPlan(open, role, channel), { channel: false, role: false, everyone: true });
  assert.deepEqual(lockPlan(locked, role, channel), { channel: false, role: false, everyone: false });
});

test('isKickExempt: owner, bots, administrators, moderators, immune roles; a plain member is not', () => {
  const guild = mkGuild();
  const plain = mkMember(guild, 'u1');
  assert.equal(isKickExempt(plain, {}), false);
  assert.equal(isKickExempt({ ...plain, id: 'O' }, {}), true);
  assert.equal(isKickExempt(mkMember(guild, 'u2', { bot: true }), {}), true);
  assert.equal(isKickExempt(mkMember(guild, 'u3', { permissions: perms('Administrator') }), {}), true);
  assert.equal(isKickExempt(mkMember(guild, 'u4', { permissions: perms('KickMembers') }), {}), true);
  addRole(guild, 'IM');
  assert.equal(isKickExempt(mkMember(guild, 'u5', { roleIds: ['IM'] }), { immuneRoleIds: ['IM'] }), true);
  addRole(guild, 'MOD');
  assert.equal(isKickExempt(mkMember(guild, 'u6', { roleIds: ['MOD'] }), { modRoleIds: ['MOD'] }), true);
});

test('checkVerifiedRole', () => {
  const guild = mkGuild();
  const plain = addRole(guild, 'P', { position: 2 });
  assert.deepEqual(checkVerifiedRole(guild, plain, {}), { ok: true });
  const mod = addRole(guild, 'M', { position: 2 });
  assert.match(checkVerifiedRole(guild, mod, { modRoleIds: ['M'] }).reason, /moderator role/);
  const imm = addRole(guild, 'I', { position: 2 });
  assert.match(checkVerifiedRole(guild, imm, { immuneRoleIds: ['I'] }).reason, /immune role/);
  const mute = addRole(guild, 'MU', { position: 2 });
  assert.match(checkVerifiedRole(guild, mute, { muteRoleId: 'MU' }).reason, /mute role/);
  const dangerous = addRole(guild, 'D', { position: 2, permissions: perms('ManageMessages') });
  const dr = checkVerifiedRole(guild, dangerous, {});
  assert.equal(dr.ok, false);
  assert.match(dr.reason, /roles with these permissions are refused/);
  const high = addRole(guild, 'H', { position: 10 });
  assert.match(checkVerifiedRole(guild, high, {}).reason, /Move my highest role/);
  assert.equal(checkVerifiedRole(guild, undefined, {}).ok, false);
});

test('discordErrorText and progressText', () => {
  const cases = {
    50013: 'I am missing a permission for that.', 50001: "I can't access that channel.", 10003: 'The channel no longer exists.',
    10011: 'The role no longer exists.', 10008: 'The message no longer exists.',
    30013: 'This server has reached the maximum number of channels or roles.',
  };
  for (const [code, text] of Object.entries(cases)) assert.equal(discordErrorText({ code: Number(code) }), text);
  assert.equal(discordErrorText(new Error('boom')), 'boom');
  assert.equal(discordErrorText(null), 'Unknown error');
  assert.match(progressText({ done: 1000, total: 2000, ok: 900, failed: 5, left: 3 }), /^\*\*1,000\/2,000\*\* processed · ✅ 900 given · ⚠️ 5 failed · 🚪 3 left$/);
});

test('MSG: strings and fresh factories', () => {
  assert.equal(typeof MSG.MODULE_OFF, 'string');
  assert.match(MSG.MODULE_OFF, /module is turned off/);
  assert.equal(typeof MSG.FONT_MISSING, 'string');
  const a = MSG.ALREADY();
  assert.deepEqual(a.components, []);
  assert.deepEqual(a.attachments, []);
  assert.equal(a.content, '');
  assert.notEqual(MSG.ALREADY(), a);
  assert.match(embedText(MSG.VERIFIED({ name: 'Cool' }).embeds[0].toJSON()), /Cool/);
});

// ── 18. Captcha image ────────────────────────────────────────────────────────

test('renderCaptchaImage: a small PNG, quickly; the font is available', async () => {
  assert.equal(isImageCaptchaAvailable(), true);
  const t0 = Date.now();
  const buf = await renderCaptchaImage('7KX4PM');
  assert.ok(Date.now() - t0 < 2000);
  assert.equal(buf.subarray(0, 4).toString('hex'), '89504e47');
  assert.ok(buf.length <= 200 * 1024);
  // The file is not tagged with the answer (no text chunks that carry the code).
  assert.ok(!buf.includes(Buffer.from('7KX4PM')));
});

// ── 19-23. Declarations ──────────────────────────────────────────────────────

test('plugin shape and collectPlugins', async () => {
  assert.equal(plugin.id, 'captcha');
  assert.equal(plugin.module, 'captcha');
  const hooks = plugin.hooks.map((h) => `${h.event}:${h.id}`);
  assert.deepEqual(hooks, ['guildMemberAdd:captcha', 'guildMemberRemove:captcha', 'guildMemberUpdate:captcha']);
  assert.equal(plugin.hooks.find((h) => h.event === 'guildMemberRemove').always, true);
  const button = plugin.components.find((c) => c.type === 'button');
  const modal = plugin.components.find((c) => c.type === 'modal');
  assert.equal(button.prefix, 'captcha');
  assert.equal(modal.prefix, 'captcha-modal');
  assert.equal(modal.module, null);
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['captcha.timeout']);
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /captcha/.test(e)), []);
  assert.ok(plugins.some((p) => p.id === 'captcha'));
});

test('guildMemberUpdate filter does not throw and passes when the settings memo is empty', () => {
  const hook = plugin.hooks.find((h) => h.event === 'guildMemberUpdate');
  const guildId = GID(999);
  assert.equal(peekSettings(guildId), null);
  const ctx = { guildId, oldMember: { roles: { cache: new Set() } }, newMember: { id: UID(1), roles: { cache: new Set() } }, oldKnown: true };
  assert.equal(hook.filter(ctx), true);
  const join = plugin.hooks.find((h) => h.event === 'guildMemberAdd');
  assert.equal(join.filter({ guildId }), true);
  const leave = plugin.hooks.find((h) => h.event === 'guildMemberRemove');
  assert.equal(leave.filter({ guildId }), true);
});

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
};

test('/captcha payload: 11 subcommands, within limits, exports', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'captcha');
  assert.equal(json.options.length, 11);
  assert.deepEqual(json.options.map((o) => o.name),
    ['setup', 'enable', 'disable', 'status', 'mode', 'role', 'channel', 'timeout', 'message', 'lock', 'verify-existing']);
  assert.ok(charCount(json) <= 1450, `${charCount(json)} characters`);
  const walk = (n) => { assert.ok(n.description.length <= 100, n.name); (n.options ?? []).forEach(walk); };
  walk(json);
  assert.equal(cmd.module, 'captcha');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
});

test('CUSTOM_IDS are static', () => {
  assert.equal(CUSTOM_IDS.start, 'captcha:start');
  assert.equal(CUSTOM_IDS.answer, 'captcha:answer');
  assert.equal(CUSTOM_IDS.modalAnswer, 'captcha-modal:answer');
  assert.equal(CUSTOM_IDS.modalPanel, 'captcha-modal:panel');
  assert.equal(CUSTOM_IDS.answerField, 'captcha-answer');
  assert.equal(timeoutJobKey('G', 'U'), 'captcha.timeout:G:U');
});

// ── Panel, lock and diagnostics against fakes ────────────────────────────────

test('publishPanel: posts, edits in place, re-posts when the old panel is gone, deletes the old one on a move, maps permission errors', async () => {
  const guild = mkGuild();
  const a = mkChannel(guild, 'CA');
  const settings = { captchaTemplate: null };

  const first = await publishPanel(guild, a, settings, {});
  assert.equal(a.sent.length, 1);
  assert.equal(a.sent[0].components.at(-1).toJSON().components[0].custom_id, 'captcha:start');

  const same = await publishPanel(guild, a, settings, { channelId: 'CA', messageId: first.id });
  assert.equal(same.id, first.id);
  assert.equal(first.edits.length, 1);
  assert.equal(first.edits[0].content, '');
  assert.equal(a.sent.length, 1);

  const gone = await publishPanel(guild, a, settings, { channelId: 'CA', messageId: 'missing' });
  assert.notEqual(gone.id, first.id);
  assert.equal(a.sent.length, 2);

  const b = mkChannel(guild, 'CB');
  await publishPanel(guild, b, settings, { channelId: 'CA', messageId: first.id });
  assert.deepEqual(a.deleted, [first.id]);

  const forbidden = mkChannel(guild, 'CC', { sendError: apiError(50013) });
  await assert.rejects(() => publishPanel(guild, forbidden, settings, {}), (e) => e instanceof CaptchaConfigError && /Cannot Post Panel/.test(e.title));
  const broken = mkChannel(guild, 'CD', { sendError: apiError(500, 'server exploded') });
  await assert.rejects(() => publishPanel(guild, broken, settings, {}), /server exploded/);
});

test('refreshPanel results', async () => {
  const guild = mkGuild();
  assert.equal((await refreshPanel(guild, {})).ok, false);
  assert.match((await refreshPanel(guild, { captchaChannelId: 'X', captchaMessageId: 'M' })).reason, /channel was deleted/);
  const ch = mkChannel(guild, 'CA');
  assert.match((await refreshPanel(guild, { captchaChannelId: 'CA', captchaMessageId: 'M' })).reason, /message was deleted/);
  const msg = await ch.send({});
  assert.deepEqual(await refreshPanel(guild, { captchaChannelId: 'CA', captchaMessageId: msg.id, captchaTemplate: null }), { ok: true });
  assert.equal(msg.edits.length, 1);
});

test('applyLock and removeLock', async () => {
  const guild = mkGuild();
  const role = addRole(guild, 'RV', { position: 2 });
  const channel = mkChannel(guild, 'C1');
  role.setPermissions = async (bits) => { role.permissions = new PermissionsBitField(bits); };
  guild.roles.everyone.setPermissions = async (bits) => { guild.roles.everyone.permissions = new PermissionsBitField(bits); };

  const plan = lockPlan(guild, role, channel);
  const res = await applyLock(guild, { role, channel, plan, reason: 'r' });
  assert.deepEqual(res.done, ['verification channel opened to @everyone', 'View Channels given to the verified role', 'View Channels removed from @everyone']);
  assert.equal(res.error, undefined);
  assert.equal(isLocked(guild), true);
  assert.equal(role.permissions.has(PermissionFlagsBits.ViewChannel), true);
  assert.equal(channel.permissionOverwrites.edited[0].o.ViewChannel, true);

  // Locked server: unlock restores View Channels.
  assert.deepEqual(await removeLock(guild, 'r'), {});
  assert.equal(isLocked(guild), false);

  // A failure stops the sequence and reports a friendly error without throwing.
  const g2 = mkGuild();
  const r2 = addRole(g2, 'RV', { position: 2 });
  const c2 = mkChannel(g2, 'C1');
  r2.setPermissions = async () => { throw apiError(50013); };
  const bad = await applyLock(g2, { role: r2, channel: c2, plan: lockPlan(g2, r2, c2), reason: 'r' });
  assert.deepEqual(bad.done, ['verification channel opened to @everyone']);
  assert.match(bad.error, /Manage Roles/);
  assert.equal(isLocked(g2), false);
});

test('diagnoseCaptcha reports setup problems and never throws', async () => {
  const guild = mkGuild();
  const none = await diagnoseCaptcha(guild, { captchaMode: 'BUTTON', captchaTimeoutMinutes: 0 });
  assert.ok(none.some((p) => /No verified role/.test(p)));
  assert.ok(none.some((p) => /No verification channel/.test(p)));

  const role = addRole(guild, 'RV', { position: 2 });
  const ch = mkChannel(guild, 'CA');
  const msg = await ch.send({});
  const ok = await diagnoseCaptcha(guild, {
    captchaRoleId: role.id, captchaChannelId: ch.id, captchaMessageId: msg.id, captchaMode: 'BUTTON', captchaTimeoutMinutes: 10,
  });
  assert.deepEqual(ok, []);

  const locked = mkGuild({ everyonePerms: perms('SendMessages') });
  const lch = mkChannel(locked, 'CA', { missing: ['ViewChannel'] });
  void lch;
  const lockedProblems = await diagnoseCaptcha(locked, { captchaRoleId: 'GONE', captchaChannelId: 'CA', captchaMessageId: 'M', captchaMode: 'BUTTON', captchaTimeoutMinutes: 0 }, { checkPanel: false });
  assert.ok(lockedProblems.some((p) => /⛔ The verified role was deleted/.test(p)));

  // A broken guild object: still no throw.
  assert.ok(Array.isArray(await diagnoseCaptcha({}, { captchaRoleId: 'x' })));
});

// ── Hook bodies and the challenge flow against a real database ────────────────

async function dbGuild(n, settings = {}) {
  const id = GID(n);
  DB_GUILDS.push(id);
  await prisma.guildSettings.deleteMany({ where: { guildId: id } });
  await updateSettings(id, {
    captchaEnabled: true, captchaMode: 'BUTTON', captchaRoleId: 'RV', captchaChannelId: 'CA', captchaMessageId: 'MSG',
    captchaTimeoutMinutes: 0, autoroleIds: [], ...settings,
  });
  const guild = mkGuild({ id });
  addRole(guild, 'RV', { position: 2 });
  return guild;
}

const row = (guild, userId) => prisma.captchaVerification.findUnique({ where: { guildId_userId: { guildId: guild.id, userId } } });
const jobFor = (guild, userId) => prisma.scheduledJob.findUnique({ where: { dedupeKey: timeoutJobKey(guild.id, userId) } });
const joinCtx = (guild, member) => ({
  member, guild, guildId: guild.id, user: member.user, state: Object.create(null),
  getSettings: async () => (await import('../src/services/settingsService.js')).getSettings(guild.id),
});

test('DB: join gate records a pending row, flags captchaPending first, schedules the timeout and replaces it on rejoin', { skip: skipDb }, async () => {
  const guild = await dbGuild(1, { captchaTimeoutMinutes: 30 });
  const m = mkMember(guild, UID(1), { joined: Date.now() });
  const ctx = joinCtx(guild, m);
  await onMemberJoin(ctx);
  assert.equal(ctx.state.captchaPending, true); // welcome (the next hook) sees the flag
  const r = await row(guild, m.id);
  assert.ok(r);
  assert.equal(r.answerHash, null);
  assert.equal(r.attempts, 0);
  const job = await jobFor(guild, m.id);
  assert.equal(job.type, 'captcha.timeout');
  assert.deepEqual(job.payload, { userId: m.id });
  assert.ok(Math.abs(job.runAt.getTime() - (m.joinedTimestamp + 30 * 60_000)) < 2000);

  // A live challenge is reset by a rejoin, and the job is replaced (one row only).
  await prisma.captchaVerification.update({ where: { guildId_userId: { guildId: guild.id, userId: m.id } }, data: { answerHash: 'x'.repeat(64), attempts: 2, expiresAt: new Date() } });
  await onMemberJoin(joinCtx(guild, m));
  const r2 = await row(guild, m.id);
  assert.equal(r2.answerHash, null);
  assert.equal(r2.attempts, 0);
  assert.equal(await prisma.scheduledJob.count({ where: { dedupeKey: timeoutJobKey(guild.id, m.id) } }), 1);

  // Leaving deletes the row and the job.
  await onMemberLeave({ guildId: guild.id, user: m.user, member: m });
  assert.equal(await row(guild, m.id), null);
  assert.equal(await jobFor(guild, m.id), null);
  // A partial member (no user) uses member.id; nothing to delete is fine.
  await onMemberLeave({ guildId: guild.id, member: { id: m.id } });
  await onMemberLeave({ guildId: guild.id });
});

test('DB: the join gate skips members who already have the role, a missing role and disabled settings; bots are auto-verified', { skip: skipDb }, async () => {
  const guild = await dbGuild(2, { autoroleIds: ['RA'] });
  addRole(guild, 'RA', { position: 2 });

  const has = mkMember(guild, UID(2), { roleIds: ['RV'] });
  const c1 = joinCtx(guild, has);
  await onMemberJoin(c1);
  assert.equal(c1.state.captchaPending, undefined);
  assert.equal(await row(guild, has.id), null);

  const bot = mkMember(guild, UID(3), { bot: true });
  const c2 = joinCtx(guild, bot);
  await onMemberJoin(c2);
  assert.equal(c2.state.captchaPending, undefined);
  assert.equal(await row(guild, bot.id), null);
  // The verified role first, then the autorole (welcome's gate is bypassed with force).
  assert.deepEqual(bot.roles.calls.map((c) => c.id), ['RV', 'RA']);

  await updateSettings(guild.id, { captchaRoleId: 'GONE' });
  const human = mkMember(guild, UID(4));
  const c3 = joinCtx(guild, human);
  await onMemberJoin(c3);
  assert.equal(c3.state.captchaPending, undefined);
  assert.equal(await row(guild, human.id), null);

  await updateSettings(guild.id, { captchaRoleId: 'RV', captchaEnabled: false });
  const c4 = joinCtx(guild, mkMember(guild, UID(5)));
  await onMemberJoin(c4);
  assert.equal(c4.state.captchaPending, undefined);
});

test('DB: completeVerification gives the role BEFORE the autoroles, clears the row and the job, and is exactly-once', { skip: skipDb }, async () => {
  const guild = await dbGuild(3, { autoroleIds: ['RA'], captchaTimeoutMinutes: 20 });
  addRole(guild, 'RA', { position: 2 });
  const m = mkMember(guild, UID(10));
  await onMemberJoin(joinCtx(guild, m));
  assert.ok(await row(guild, m.id));
  assert.ok(await jobFor(guild, m.id));

  const settings = await joinCtx(guild, m).getSettings();
  const role = guild.roles.cache.get('RV');
  const res = await completeVerification(m, settings, role);
  assert.equal(res.ok, true);
  assert.equal(res.added, true);
  assert.deepEqual(m.roles.calls.map((c) => c.id), ['RV', 'RA']);
  assert.equal(await row(guild, m.id), null);
  assert.equal(await jobFor(guild, m.id), null);

  // Verifying again (already has the role, no row): nothing more is granted.
  const again = await completeVerification(m, settings, role);
  assert.equal(again.ok, true);
  assert.equal(again.added, false);
  assert.deepEqual(m.roles.calls.map((c) => c.id), ['RV', 'RA']);

  // The update hook that fires for our own grant finds no pending row and does nothing.
  const ctx = { guildId: guild.id, oldMember: { roles: { cache: new Collection() } }, newMember: m, oldKnown: true, getSettings: async () => settings };
  await onMemberUpdate(ctx);
  assert.deepEqual(m.roles.calls.map((c) => c.id), ['RV', 'RA']);

  // Discord permission failures become friendly payloads, never throws.
  const m2 = mkMember(guild, UID(11));
  m2.roles.add = async () => { throw apiError(50013); };
  const denied = await completeVerification(m2, settings, role);
  assert.equal(denied.ok, false);
  assert.match(embedText(denied.payload.embeds[0].toJSON()), /missing permissions/);
  const m3 = mkMember(guild, UID(12));
  m3.roles.add = async () => { throw apiError(10011); };
  assert.equal((await completeVerification(m3, settings, role)).ok, false);
  const m4 = mkMember(guild, UID(13));
  m4.roles.add = async () => { throw apiError(500, 'transient'); };
  await assert.rejects(() => completeVerification(m4, settings, role), /transient/);
});

test('DB: a role given by someone else verifies a pending member once, and ignores everyone else', { skip: skipDb }, async () => {
  const guild = await dbGuild(4, { autoroleIds: ['RA'], captchaTimeoutMinutes: 20 });
  addRole(guild, 'RA', { position: 2 });
  const pendingMember = mkMember(guild, UID(20));
  await onMemberJoin(joinCtx(guild, pendingMember));
  const settings = await joinCtx(guild, pendingMember).getSettings();

  const old = { roles: { cache: new Collection() } };
  pendingMember.roles.cache.set('RV', guild.roles.cache.get('RV')); // the moderator's role grant
  const ctx = { guildId: guild.id, oldMember: old, newMember: pendingMember, oldKnown: true, getSettings: async () => settings };
  await onMemberUpdate(ctx);
  assert.equal(await row(guild, pendingMember.id), null);
  assert.equal(await jobFor(guild, pendingMember.id), null);
  assert.deepEqual(pendingMember.roles.calls.map((c) => c.id), ['RA']); // autoroles granted once

  await onMemberUpdate(ctx); // the same event twice: the row is gone, nothing more happens
  assert.deepEqual(pendingMember.roles.calls.map((c) => c.id), ['RA']);

  // A member with no pending row (verify-existing, a role shuffle) gets nothing.
  const other = mkMember(guild, UID(21), { roleIds: ['RV'] });
  await onMemberUpdate({ guildId: guild.id, oldMember: old, newMember: other, oldKnown: true, getSettings: async () => settings });
  assert.deepEqual(other.roles.calls, []);

  // Still in Rules Screening: welcome's screening watch is scheduled instead of forcing the roles now.
  const screening = mkMember(guild, UID(22), { pending: true });
  await onMemberJoin(joinCtx(guild, screening));
  screening.roles.cache.set('RV', guild.roles.cache.get('RV'));
  await onMemberUpdate({ guildId: guild.id, oldMember: old, newMember: screening, oldKnown: true, getSettings: async () => settings });
  assert.deepEqual(screening.roles.calls, []);
  const watch = await prisma.scheduledJob.findUnique({ where: { dedupeKey: `welcome.autorole:${guild.id}:${screening.id}` } });
  assert.ok(watch);
  await prisma.scheduledJob.deleteMany({ where: { guildId: guild.id } });
});

test('DB: Verify in BUTTON mode verifies in one click', { skip: skipDb }, async () => {
  const guild = await dbGuild(5, { autoroleIds: ['RA'] });
  addRole(guild, 'RA', { position: 2 });
  const m = mkMember(guild, UID(30));
  await onMemberJoin(joinCtx(guild, m));
  const i = mkInteraction(guild, m, { customId: 'captcha:start' });
  await handleCaptchaButton(i);
  assert.deepEqual(i.log.defers, [['reply', { ephemeral: true }]]); // acknowledged first, privately
  assert.match(embedText(lastEmbed(i.log.edits)), /Verified/);
  assert.deepEqual(m.roles.calls.map((c) => c.id), ['RV', 'RA']);
  assert.equal(await row(guild, m.id), null);

  // Pressing it again: already verified.
  const j = mkInteraction(guild, m, { customId: 'captcha:start' });
  await handleCaptchaButton(j);
  assert.match(embedText(lastEmbed(j.log.edits)), /Already Verified/);
});

test('DB: Verify refuses while the module is off, the role is unusable, or Rules Screening is pending', { skip: skipDb }, async () => {
  const guild = await dbGuild(6);
  const m = mkMember(guild, UID(31), { pending: true });
  const screening = mkInteraction(guild, m);
  await handleCaptchaButton(screening);
  assert.match(embedText(lastEmbed(screening.log.edits)), /Rules Screening First/);
  assert.deepEqual(m.roles.calls, []);

  await updateSettings(guild.id, { captchaRoleId: 'GONE' });
  const gone = mkInteraction(guild, mkMember(guild, UID(32)));
  await handleCaptchaButton(gone);
  assert.match(embedText(lastEmbed(gone.log.edits)), /Verification Unavailable/);

  await updateSettings(guild.id, { captchaRoleId: 'RV', captchaEnabled: false });
  const off = mkInteraction(guild, mkMember(guild, UID(33)));
  await handleCaptchaButton(off);
  assert.match(off.log.edits.at(-1).content, /module is turned off/);
  assert.deepEqual(off.log.edits.at(-1).embeds, []);
});

test('DB: Verify in IMAGE mode issues a challenge that stores only a hash and leaks nothing', { skip: skipDb }, async () => {
  const guild = await dbGuild(7, { captchaMode: 'IMAGE' });
  const m = mkMember(guild, UID(40));
  const i = mkInteraction(guild, m, { customId: 'captcha:start' });
  await handleCaptchaButton(i);
  const payload = i.log.edits.at(-1);
  assert.equal(payload.files.length, 1);
  assert.equal(payload.files[0].name, 'captcha.png');
  assert.equal(payload.files[0].description ?? undefined, undefined);
  assert.deepEqual(payload.components[0].toJSON().components.map((c) => c.custom_id), ['captcha:answer', 'captcha:start']);
  assert.deepEqual(payload.attachments, []);

  const r = await row(guild, m.id);
  assert.match(r.answerHash, /^[0-9a-f]{64}$/);
  assert.equal(r.attempts, 0);
  assert.ok(r.expiresAt.getTime() > Date.now() + 9 * 60_000);
  // Nothing the member receives repeats the stored value or looks like the code.
  assert.ok(!JSON.stringify(payload.embeds.map((e) => e.toJSON())).includes(r.answerHash));
  assert.doesNotMatch(JSON.stringify(payload.embeds.map((e) => e.toJSON())), /\b[ACEFHJKMNPRTWXY34679]{6}\b/);

  // "Enter code" opens a modal as the first and only response.
  const a = mkInteraction(guild, m, { customId: 'captcha:answer', fromEphemeral: true });
  await handleCaptchaButton(a);
  assert.equal(a.log.modals.length, 1);
  const modal = a.log.modals[0].toJSON();
  assert.equal(modal.custom_id, 'captcha-modal:answer');
  assert.equal(modal.components[0].components[0].custom_id, 'captcha-answer');
  assert.deepEqual(a.log.defers, []);

  // "New image" from the ephemeral challenge updates in place.
  const n = mkInteraction(guild, m, { customId: 'captcha:start', fromEphemeral: true });
  await handleCaptchaButton(n);
  assert.deepEqual(n.log.defers, [['update']]);
  assert.equal(n.log.edits.at(-1).files.length, 1);
  assert.notEqual((await row(guild, m.id)).answerHash, r.answerHash);
});

test('DB: the answer form: wrong tries count down, the third wrong tries issues a fresh code, the right code verifies', { skip: skipDb }, async () => {
  const guild = await dbGuild(8, { captchaMode: 'IMAGE', autoroleIds: ['RA'] });
  addRole(guild, 'RA', { position: 2 });
  const m = mkMember(guild, UID(50));
  const key = { guildId_userId: { guildId: guild.id, userId: m.id } };
  const plant = (code, over = {}) => prisma.captchaVerification.upsert({
    where: key,
    create: { guildId: guild.id, userId: m.id, answerHash: hashAnswer(guild.id, m.id, code), attempts: 0, expiresAt: new Date(Date.now() + 600_000), ...over },
    update: { answerHash: hashAnswer(guild.id, m.id, code), attempts: 0, expiresAt: new Date(Date.now() + 600_000), ...over },
  });
  const submit = async (input) => {
    const i = mkInteraction(guild, m, { customId: 'captcha-modal:answer', input });
    await handleCaptchaModal(i);
    return i;
  };

  await plant('ACEFHJ');
  let i = await submit('wrong1');
  assert.deepEqual(i.log.defers, [['update']]);
  assert.match(i.log.edits.at(-1).content, /\*\*2\*\* tries left/);
  assert.equal(i.log.edits.at(-1).embeds, undefined); // only the text changes: the image stays
  assert.equal((await row(guild, m.id)).attempts, 1);

  i = await submit('AAAAAA');
  assert.match(i.log.edits.at(-1).content, /\*\*1\*\* try left/);
  assert.equal((await row(guild, m.id)).attempts, 2);

  const before = (await row(guild, m.id)).answerHash;
  i = await submit('CCCCCC');
  assert.match(i.log.edits.at(-1).content, /Too many wrong tries/);
  assert.equal(i.log.edits.at(-1).files.length, 1);
  const fresh = await row(guild, m.id);
  assert.notEqual(fresh.answerHash, before);
  assert.equal(fresh.attempts, 0);

  // Expired codes are replaced too.
  await plant('ACEFHJ', { expiresAt: new Date(Date.now() - 1000) });
  i = await submit('ACEFHJ');
  assert.match(i.log.edits.at(-1).content, /expired/);
  assert.equal(i.log.edits.at(-1).files.length, 1);

  // A submit with no live code gets a new image.
  await prisma.captchaVerification.deleteMany({ where: { guildId: guild.id, userId: m.id } });
  i = await submit('ACEFHJ');
  assert.match(i.log.edits.at(-1).content, /no longer valid/);

  // The right code, in any case and spacing.
  await plant('ACEFHJ');
  i = await submit(' ace-fhj ');
  assert.match(embedText(lastEmbed(i.log.edits)), /Verified/);
  assert.deepEqual(m.roles.calls.map((c) => c.id), ['RV', 'RA']);
  assert.equal(await row(guild, m.id), null);

  // A stale answer cannot hit a newer challenge: the attempt increment is matched on the hash.
  await plant('ACEFHJ');
  const stale = hashAnswer(guild.id, m.id, 'OLDOLD');
  const { count } = await prisma.captchaVerification.updateMany({ where: { guildId: guild.id, userId: m.id, answerHash: stale }, data: { attempts: { increment: 1 } } });
  assert.equal(count, 0);
  assert.equal((await row(guild, m.id)).attempts, 0);
});

test('DB: the answer form checks the module, Rules Screening and mode changes', { skip: skipDb }, async () => {
  const guild = await dbGuild(9, { captchaMode: 'IMAGE' });
  const m = mkMember(guild, UID(60), { pending: true });
  await prisma.captchaVerification.create({
    data: { guildId: guild.id, userId: m.id, answerHash: hashAnswer(guild.id, m.id, 'ACEFHJ'), attempts: 0, expiresAt: new Date(Date.now() + 600_000) },
  });
  const i = mkInteraction(guild, m, { customId: 'captcha-modal:answer', input: 'ACEFHJ' });
  await handleCaptchaModal(i);
  assert.match(embedText(lastEmbed(i.log.edits)), /Rules Screening First/);
  assert.deepEqual(m.roles.calls, []);
  assert.ok(await row(guild, m.id)); // the code stays valid

  await updateSettings(guild.id, { captchaMode: 'BUTTON' });
  const b = mkInteraction(guild, mkMember(guild, UID(61)), { customId: 'captcha-modal:answer', input: 'ACEFHJ' });
  await handleCaptchaModal(b);
  assert.match(b.log.edits.at(-1).content, /Press \*\*Verify\*\*/);

  await updateSettings(guild.id, { captchaEnabled: false });
  const off = mkInteraction(guild, mkMember(guild, UID(62)), { customId: 'captcha-modal:answer', input: 'ACEFHJ' });
  await handleCaptchaModal(off);
  assert.deepEqual(off.log.defers, []); // answered immediately, never deferred
  assert.match(off.log.replies.at(-1).content, /module is turned off/);
  assert.equal(off.log.replies.at(-1).ephemeral, true);

  const unknown = mkInteraction(guild, m, { customId: 'captcha-modal:nope' });
  await handleCaptchaModal(unknown);
  assert.match(unknown.log.replies.at(-1).content, /expired/);
});

test('DB: the panel form is admin-only and works while the module is off', { skip: skipDb }, async () => {
  const guild = await dbGuild(10, { captchaEnabled: false });
  const member = mkMember(guild, UID(70));
  const i = mkInteraction(guild, member, { customId: 'captcha-modal:panel' });
  await handleCaptchaModal(i);
  assert.equal(i.log.replies.at(-1).ephemeral, true);
  assert.match(embedText(i.log.replies.at(-1).embeds[0].toJSON()), /Only server admins/);
  assert.deepEqual(i.log.defers, []);
});

test('DB: issueChallenge stores a fresh hash and returns the challenge message with the drawn image', { skip: skipDb }, async () => {
  const guild = await dbGuild(11, { captchaMode: 'IMAGE' });
  const m = mkMember(guild, UID(80));
  const settings = await joinCtx(guild, m).getSettings();
  const payload = await issueChallenge(m, settings, { content: 'hello' });
  assert.equal(payload.content, 'hello');
  const first = await row(guild, m.id);
  assert.ok(first.answerHash);
  assert.ok(payload.files[0].attachment.length > 1000);
});

// ── Timeout job ──────────────────────────────────────────────────────────────

function fakeMod(result) {
  const calls = [];
  const fn = async (input) => { calls.push(input); return typeof result === 'function' ? result(input) : result; };
  fn.calls = calls;
  return fn;
}

async function timeoutSetup(n, over = {}) {
  const guild = await dbGuild(n, { captchaTimeoutMinutes: 30, ...over });
  const m = mkMember(guild, UID(n * 10));
  guild.members.fetch = async ({ user }) => {
    const found = guild.members.cache.get(user);
    if (!found) throw apiError(10007);
    return found;
  };
  await prisma.captchaVerification.create({ data: { guildId: guild.id, userId: m.id } });
  return { guild, m, job: { payload: { userId: m.id } } };
}

test('DB: captcha.timeout: kicks through moderation after a DM, with the contract arguments, and deletes the row', { skip: skipDb }, async () => {
  const { guild, m, job } = await timeoutSetup(20);
  const mod = fakeMod({ ok: true });
  await runTimeoutJob(job, { guild, attempt: 1 }, { executeModAction: mod });
  assert.equal(mod.calls.length, 1);
  const call = mod.calls[0];
  assert.equal(call.guild, guild);
  assert.equal(call.target, m);
  assert.equal(call.moderator, guild.client.user);
  assert.equal(call.action, 'KICK');
  assert.equal(call.source, 'captcha');
  assert.equal(call.silent, true);
  assert.equal(call.skipEscalation, true);
  assert.ok(call.reason.length <= 500);
  assert.match(call.reason, /^Captcha: not verified within 30 minutes$/);
  assert.equal(m.dms.length, 1); // the friendly DM
  assert.deepEqual(m.dms[0].allowedMentions, { parse: [] });
  assert.equal(await row(guild, m.id), null);
});

test('DB: captcha.timeout: a scheduler retry does not DM twice', { skip: skipDb }, async () => {
  const { guild, m, job } = await timeoutSetup(21);
  await runTimeoutJob(job, { guild, attempt: 2 }, { executeModAction: fakeMod({ ok: true }) });
  assert.equal(m.dms.length, 0);
});

test('DB: captcha.timeout: skip paths (module off, never, no role, no row, verified, exempt, not kickable)', { skip: skipDb }, async () => {
  const { guild, m, job } = await timeoutSetup(22);
  const mod = fakeMod({ ok: true });
  const run = () => runTimeoutJob(job, { guild, attempt: 1 }, { executeModAction: mod });

  await updateSettings(guild.id, { captchaEnabled: false });
  await run();
  await updateSettings(guild.id, { captchaEnabled: true, captchaTimeoutMinutes: 0 });
  await run();
  await updateSettings(guild.id, { captchaTimeoutMinutes: 30, captchaRoleId: 'GONE' });
  await run();
  assert.equal(mod.calls.length, 0);
  assert.ok(await row(guild, m.id)); // the row stays in all three cases
  await updateSettings(guild.id, { captchaRoleId: 'RV' });

  // Exempt: a member with Administrator is never kicked, and nobody is DMed.
  m.permissions = perms('Administrator');
  await run();
  assert.equal(mod.calls.length, 0);
  assert.equal(m.dms.length, 0);
  m.permissions = perms();

  // The bot cannot kick them: skipped without a DM.
  m.kickable = false;
  await run();
  assert.equal(mod.calls.length, 0);
  assert.equal(m.dms.length, 0);
  m.kickable = true;

  // They verified while no event reached us: the row goes, no kick.
  m.roles.cache.set('RV', guild.roles.cache.get('RV'));
  await run();
  assert.equal(mod.calls.length, 0);
  assert.equal(await row(guild, m.id), null);

  // No row at all: nothing happens.
  m.roles.cache.delete('RV');
  await run();
  assert.equal(mod.calls.length, 0);
});

test('DB: captcha.timeout: member gone (10007) deletes the row; a transient fetch error retries', { skip: skipDb }, async () => {
  const { guild, m, job } = await timeoutSetup(23);
  const mod = fakeMod({ ok: true });
  guild.members.fetch = async () => { throw apiError(500, 'gateway hiccup'); };
  await assert.rejects(() => runTimeoutJob(job, { guild, attempt: 1 }, { executeModAction: mod }), /gateway hiccup/);
  assert.ok(await row(guild, m.id));
  guild.members.fetch = async () => { throw apiError(10007); };
  await runTimeoutJob(job, { guild, attempt: 1 }, { executeModAction: mod });
  assert.equal(await row(guild, m.id), null);
  assert.equal(mod.calls.length, 0);
});

test('DB: captcha.timeout: every moderation result code', { skip: skipDb }, async () => {
  const cases = [
    [{ ok: false, code: 'IMMUNE', error: 'immune' }, 'kept'],
    [{ ok: false, code: 'HIERARCHY', error: 'h' }, 'kept'],
    [{ ok: false, code: 'NOT_PERMITTED', error: 'p' }, 'kept'],
    [{ ok: false, code: 'NOT_IN_GUILD', error: 'n' }, 'deleted'],
    [{ ok: false, code: 'DISCORD_ERROR', error: 'd', applied: true }, 'deleted'],
    [{ ok: false, code: 'DISCORD_ERROR', error: 'd' }, 'throws'],
    [{ ok: false, code: 'INVALID', error: 'i' }, 'throws'],
    [{ ok: false, code: 'ALREADY', error: 'a' }, 'throws'],
  ];
  for (const [i, [result, expect]] of cases.entries()) {
    const { guild, m, job } = await timeoutSetup(30 + i);
    const mod = fakeMod(result);
    const go = () => runTimeoutJob(job, { guild, attempt: 1 }, { executeModAction: mod });
    if (expect === 'throws') await assert.rejects(go);
    else await go();
    assert.equal(mod.calls.length, 1, result.code);
    assert.equal(Boolean(await row(guild, m.id)), expect !== 'deleted', `${result.code} ${expect}`);
  }
});

test('captcha.timeout: a malformed payload is a permanent failure', async () => {
  const guild = mkGuild({ id: GID(99) });
  for (const payload of [undefined, {}, { userId: 5 }, { userId: 'abc' }, { userId: '123' }]) {
    await assert.rejects(() => runTimeoutJob({ payload }, { guild }), (e) => e instanceof PermanentJobError);
  }
});

// ── verify-existing ──────────────────────────────────────────────────────────

function bulkInteraction(guild, adminMember) {
  const i = mkSlash(guild, adminMember, 'verify-existing');
  i.channel = null;
  return i;
}

async function waitForBatch(guild) {
  const deadline = Date.now() + 20_000;
  while (getBulkRun(guild.id) && Date.now() < deadline) await new Promise((r) => { const t = setImmediate(r); t.unref?.(); });
  assert.equal(getBulkRun(guild.id), null, 'the batch finished');
}

test('DB: verify-existing: skips holders, gives bots the role, spares pending members unless asked, prunes stale rows', { skip: skipDb }, async () => {
  const guild = await dbGuild(40, { autoroleIds: ['RA'], captchaTimeoutMinutes: 10 });
  addRole(guild, 'RA', { position: 2 });
  const admin = mkMember(guild, UID(400), { roleIds: ['RV'], permissions: perms('Administrator') });
  admin.roles.highest = { position: 9 };
  const plain = mkMember(guild, UID(401));
  const bot = mkMember(guild, UID(402), { bot: true });
  const waiting = mkMember(guild, UID(403));
  guild.memberCount = guild.members.cache.size;
  await onMemberJoin(joinCtx(guild, waiting)); // pending row + timeout job
  await prisma.captchaVerification.create({ data: { guildId: guild.id, userId: UID(499) } }); // left while the bot was away
  const settings = await joinCtx(guild, admin).getSettings();

  const i = bulkInteraction(guild, admin);
  await startVerifyExisting(i, settings, { includePending: false });
  assert.ok(getBulkRun(guild.id));
  assert.equal(isBulkTarget(guild.id, plain.id), true);
  assert.equal(isBulkTarget(guild.id, waiting.id), false);
  await waitForBatch(guild);
  assert.deepEqual(plain.roles.calls.map((c) => c.id), ['RV']);
  assert.deepEqual(bot.roles.calls.map((c) => c.id), ['RV']);
  assert.deepEqual(waiting.roles.calls, []);
  assert.equal(await row(guild, UID(499)), null); // stale row pruned
  assert.ok(await row(guild, waiting.id)); // pending member untouched
  const final = lastEmbed(i.log.followUps);
  assert.match(final.description, /2\/2/);
  assert.match(final.description, /Gave <@&RV> to 2 member\(s\)/);
  // The summary is posted once: the progress reply is deleted rather than edited into a second copy.
  assert.equal(i.log.followUps.length, 1);
  assert.equal(i.log.deleted, 1);
  assert.equal(i.log.edits.some((p) => p.embeds?.[0]?.toJSON?.().title?.includes('Existing Members Verified')), false);

  // include_pending finishes their verification, with the autoroles after the role.
  const j = bulkInteraction(guild, admin);
  await startVerifyExisting(j, settings, { includePending: true });
  await waitForBatch(guild);
  assert.deepEqual(waiting.roles.calls.map((c) => c.id), ['RV', 'RA']);
  assert.equal(await row(guild, waiting.id), null);
  assert.equal(await jobFor(guild, waiting.id), null);

  // Nothing left to do.
  const k = bulkInteraction(guild, admin);
  await startVerifyExisting(k, settings, {});
  assert.match(embedText(lastEmbed(k.log.edits)), /Nothing To Do/);
  assert.equal(getBulkRun(guild.id), null);
});

test('DB: verify-existing: at most one run per guild, 2,000 members per run, "run again" message, the 100,000 cap', { skip: skipDb }, async () => {
  const guild = await dbGuild(41);
  const admin = mkMember(guild, UID(410), { roleIds: ['RV'], permissions: perms('Administrator') });
  admin.roles.highest = { position: 9 };
  const total = VERIFY_EXISTING_BATCH + 5;
  for (let n = 0; n < total; n++) {
    const id = `95502944444${String(n).padStart(7, '0')}`;
    guild.members.cache.set(id, {
      id, user: { id, bot: false }, pending: false, guild,
      roles: { cache: new Collection(), add: async function add(rid) { this.cache.set(rid, { id: rid }); return { id, pending: false, guild, roles: this }; } },
    });
  }
  guild.memberCount = guild.members.cache.size;
  const settings = await joinCtx(guild, admin).getSettings();

  const i = bulkInteraction(guild, admin);
  await startVerifyExisting(i, settings, {});
  assert.equal(getBulkRun(guild.id).total, VERIFY_EXISTING_BATCH);
  const dup = bulkInteraction(guild, admin);
  await startVerifyExisting(dup, settings, {});
  assert.match(embedText(lastEmbed(dup.log.edits)), /Already Running/);
  await waitForBatch(guild);
  const final = lastEmbed(i.log.followUps);
  assert.match(final.description, /5 more still need it: run `\/captcha verify-existing` again to continue/);
  const done = [...guild.members.cache.values()].filter((m) => m.roles.cache.has('RV')).length;
  assert.equal(done, VERIFY_EXISTING_BATCH + 1); // the admin already had it

  // A guild over the cap is refused.
  const big = mkGuild({ id: GID(42) });
  addRole(big, 'RV', { position: 2 });
  big.memberCount = 100_001;
  const bigAdmin = mkMember(big, UID(420), { permissions: perms('Administrator') });
  bigAdmin.roles.highest = { position: 9 };
  const bi = bulkInteraction(big, bigAdmin);
  await startVerifyExisting(bi, { captchaRoleId: 'RV' }, {});
  assert.match(embedText(lastEmbed(bi.log.edits)), /Server Too Large/);
  assert.equal(getBulkRun(big.id), null);
});

test('DB: verify-existing: a deleted verified role aborts the batch with a warning and releases the guild', { skip: skipDb }, async () => {
  const guild = await dbGuild(43);
  const admin = mkMember(guild, UID(430), { roleIds: ['RV'], permissions: perms('Administrator') });
  admin.roles.highest = { position: 9 };
  const targets = [UID(431), UID(432), UID(433)].map((id) => mkMember(guild, id));
  targets[0].roles.add = async () => { throw apiError(10011); };
  guild.memberCount = guild.members.cache.size;
  const settings = await joinCtx(guild, admin).getSettings();

  const i = bulkInteraction(guild, admin);
  await startVerifyExisting(i, settings, {});
  await waitForBatch(guild);
  const final = lastEmbed(i.log.followUps);
  assert.match(final.title, /Batch Stopped/);
  assert.match(final.description, /The verified role was deleted\./);
  assert.equal(getBulkRun(guild.id), null);
  assert.deepEqual(targets[1].roles.calls, []); // the run stopped at the first failure
  assert.equal(targets[2].roles.calls.length, 0);
});

// ── /captcha command ─────────────────────────────────────────────────────────

test('DB: /captcha setup creates the role and channel, posts the panel and saves the settings', { skip: skipDb }, async () => {
  const id = GID(50);
  DB_GUILDS.push(id);
  await prisma.guildSettings.deleteMany({ where: { guildId: id } });
  await updateSettings(id, { captchaEnabled: false });
  const guild = mkGuild({ id });
  guild.members.me.permissions = perms('ManageRoles', 'ManageChannels', 'ViewChannel', 'SendMessages', 'EmbedLinks', 'ReadMessageHistory', 'AddReactions', 'CreatePublicThreads', 'CreatePrivateThreads', 'KickMembers');
  guild.roles.create = async (o) => {
    assert.equal(o.name, 'verified');
    assert.deepEqual(o.permissions, []);
    return addRole(guild, '95502955555000001', { position: 1 });
  };
  let channel;
  guild.channels.create = async (o) => {
    assert.equal(o.name, 'verification');
    assert.ok(o.permissionOverwrites.some((p) => p.id === id && p.deny.includes(PermissionFlagsBits.SendMessages)));
    channel = mkChannel(guild, '95502955555000002');
    return channel;
  };
  const admin = mkMember(guild, UID(500), { permissions: perms('Administrator') });
  admin.user.tag = 'admin#0';
  const i = mkSlash(guild, admin, 'setup', { mode: 'BUTTON' });
  await cmd.execute(i);
  const e = lastEmbed(i.log.edits);
  assert.match(e.title, /Verification Ready/);
  assert.match(e.description, /Created:\*\* role <@&95502955555000001>, channel <#95502955555000002>/);
  assert.match(e.description, /lock state:on/); // not locked yet
  const saved = await prisma.guildSettings.findUnique({ where: { guildId: id } });
  assert.equal(saved.captchaEnabled, true);
  assert.equal(saved.captchaRoleId, '95502955555000001');
  assert.equal(saved.captchaChannelId, channel.id);
  assert.equal(saved.captchaMode, 'BUTTON');
  assert.ok(saved.captchaMessageId);
  assert.equal(channel.sent.length, 1);

  // A missing Manage Roles permission is explained before anything is created.
  const g2 = mkGuild({ id });
  g2.members.me.permissions = perms();
  const a2 = mkMember(g2, UID(501), { permissions: perms('Administrator') });
  const i2 = mkSlash(g2, a2, 'setup');
  await cmd.execute(i2);
  assert.match(embedText(lastEmbed(i2.log.edits)), /Missing Permissions/);
});

test('DB: /captcha timeout, mode, role, enable, disable and message button options', { skip: skipDb }, async () => {
  const guild = await dbGuild(51, { captchaEnabled: false, captchaChannelId: 'CA', captchaMessageId: null });
  const ch = mkChannel(guild, 'CA');
  const panel = await ch.send({});
  await updateSettings(guild.id, { captchaMessageId: panel.id });
  const admin = mkMember(guild, UID(510), { permissions: perms('Administrator') });
  admin.roles.highest = { position: 9 };
  const run = async (sub, o) => { const i = mkSlash(guild, admin, sub, o); await cmd.execute(i); return i; };
  const settings = () => prisma.guildSettings.findUnique({ where: { guildId: guild.id } });

  // timeout
  let i = await run('timeout', { minutes: 3 });
  assert.match(embedText(lastEmbed(i.log.edits)), /Use 0 \(never\) or between 5 and 10080/);
  i = await run('timeout', { minutes: 45 });
  assert.match(embedText(lastEmbed(i.log.edits)), /Timeout Set/);
  assert.equal((await settings()).captchaTimeoutMinutes, 45);
  await prisma.scheduledJob.create({ data: { guildId: guild.id, type: 'captcha.timeout', runAt: new Date(Date.now() + 1e7), payload: { userId: UID(1) }, dedupeKey: timeoutJobKey(guild.id, UID(1)) } });
  i = await run('timeout', { minutes: 0 });
  assert.match(embedText(lastEmbed(i.log.edits)), /Timeout Off.*Cancelled 1 scheduled kick/s);
  assert.equal(await jobFor(guild, UID(1)), null);

  // mode
  i = await run('mode', { type: 'IMAGE' });
  assert.match(embedText(lastEmbed(i.log.edits)), /Verification Mode Set/);
  assert.equal((await settings()).captchaMode, 'IMAGE');

  // role
  addRole(guild, 'R2', { position: 2 });
  addRole(guild, 'RM', { position: 2, permissions: perms('ManageMessages') });
  i = await run('role', { role: guild.roles.cache.get('RM') });
  assert.match(embedText(lastEmbed(i.log.edits)), /Cannot Use This Role/);
  i = await run('role', { role: guild.roles.cache.get('RV') });
  assert.match(embedText(lastEmbed(i.log.edits)), /No Change/);
  i = await run('role', { role: guild.roles.cache.get('R2') });
  assert.match(embedText(lastEmbed(i.log.edits)), /Verified Role Set/);
  assert.match(embedText(lastEmbed(i.log.edits)), /verify-existing/);
  assert.equal((await settings()).captchaRoleId, 'R2');

  // enable / disable
  i = await run('enable');
  assert.match(embedText(lastEmbed(i.log.edits)), /Verification Enabled/);
  assert.equal((await settings()).captchaEnabled, true);
  await prisma.captchaVerification.create({ data: { guildId: guild.id, userId: UID(2) } });
  i = await run('disable');
  assert.match(embedText(lastEmbed(i.log.edits)), /1\*\* member\(s\) were still waiting/);
  assert.equal((await settings()).captchaEnabled, false);

  // message: button options edit the stored template and the panel in place
  i = await run('message', { button_label: '  Let me in  ', button_color: 'DANGER' });
  assert.match(embedText(lastEmbed(i.log.edits)), /Label \*\*Let me in\*\*, colour \*\*Red\*\*/);
  const stored = (await settings()).captchaTemplate;
  assert.deepEqual(stored.verifyButton, { label: 'Let me in', style: 'DANGER' });
  assert.equal(panel.edits.length, 1);
  assert.equal(panel.edits[0].components.at(-1).toJSON().components[0].label, 'Let me in');

  i = await run('message', { reset: true, button_label: 'x' });
  assert.match(embedText(lastEmbed(i.log.edits)), /Use reset on its own/);
  i = await run('message', { button_label: '   ' });
  assert.match(embedText(lastEmbed(i.log.edits)), /cannot be empty/);
  i = await run('message', { reset: false });
  assert.match(embedText(lastEmbed(i.log.edits)), /Choose an option/);
  i = await run('message', { reset: true });
  assert.match(embedText(lastEmbed(i.log.edits)), /Panel Reset/);
  assert.equal((await settings()).captchaTemplate, null);

  // status
  i = await run('status');
  assert.match(lastEmbed(i.log.edits).title, /Verification/);
  assert.ok(lastEmbed(i.log.edits).fields.some((f) => f.name === 'Waiting to verify' && f.value === '1'));

  // message with no options opens the form (the first response)
  const form = mkSlash(guild, admin, 'message');
  await cmd.execute(form);
  assert.equal(form.log.modals.length, 1);
  assert.equal(form.log.modals[0].toJSON().custom_id, 'captcha-modal:panel');
  assert.deepEqual(form.log.defers, []);
});

test('DB: /captcha lock: pre-checks, and lock off keeps a private server private (prior state)', { skip: skipDb }, async () => {
  const guild = await dbGuild(52, { captchaLockdownPrior: false, captchaChannelId: 'CA' });
  mkChannel(guild, 'CA');
  const admin = mkMember(guild, UID(520), { permissions: perms('Administrator') });
  const run = async (o) => { const i = mkSlash(guild, admin, 'lock', o); await cmd.execute(i); return i; };

  // The server is open: "off" says so, and a stale prior state is dropped.
  let i = await run({ state: 'off' });
  assert.match(embedText(lastEmbed(i.log.replies)), /Not Locked/);
  assert.equal((await prisma.guildSettings.findUnique({ where: { guildId: guild.id } })).captchaLockdownPrior, null);

  // The server is locked and was private before: no View Channels is granted, the prior state is cleared.
  await updateSettings(guild.id, { captchaLockdownPrior: false });
  guild.roles.everyone.permissions = perms('SendMessages');
  let granted = false;
  guild.roles.everyone.setPermissions = async () => { granted = true; };
  i = await run({ state: 'off' });
  assert.match(embedText(lastEmbed(i.log.replies)), /Lock-down Cleared/);
  assert.equal(granted, false);
  assert.equal((await prisma.guildSettings.findUnique({ where: { guildId: guild.id } })).captchaLockdownPrior, null);

  // lock on without a verified role / channel
  await updateSettings(guild.id, { captchaRoleId: null });
  i = await run({ state: 'on' });
  assert.match(embedText(lastEmbed(i.log.replies)), /Set Up First/);

  // lock on: the bot would lose sight of the server
  await updateSettings(guild.id, { captchaRoleId: 'RV' });
  guild.roles.everyone.permissions = perms('ViewChannel', 'SendMessages');
  i = await run({ state: 'on' });
  assert.match(embedText(lastEmbed(i.log.replies)), /Cannot Lock.*View Channels/s);
});

test('DB: a failing command surfaces config errors as embeds and rethrows the rest', { skip: skipDb }, async () => {
  const guild = await dbGuild(53, { captchaChannelId: 'CA' });
  const admin = mkMember(guild, UID(530), { permissions: perms('Administrator') });
  const bad = mkSlash(guild, admin, 'channel', { channel: { id: 'X', isTextBased: () => false } });
  await cmd.execute(bad);
  assert.match(embedText(lastEmbed(bad.log.edits)), /Cannot Use That Channel/);
});

// A fake confirm dialog: the "button press" arrives through a fake collector, so awaitConfirm resolves true or false.
function confirming(interaction, press = 'yes') {
  interaction.reply = async (p) => {
    interaction.log.replies.push(p);
    interaction.replied = true;
    const row = p.components?.[0]?.toJSON?.();
    const yesId = row?.components?.[0]?.custom_id;
    if (!yesId) return {};
    const nonceBase = yesId.replace(/:yes$/, '');
    const message = {
      id: 'MSG1',
      createMessageComponentCollector: () => {
        const c = new EventEmitter();
        setImmediate(() => {
          c.emit('collect', { customId: `${nonceBase}:${press}`, user: interaction.user, update: async () => {} });
          c.emit('end');
        });
        return c;
      },
    };
    return { resource: { message } };
  };
  return interaction;
}

test('DB: /captcha lock on stores the prior state, lock off restores View Channels and clears it', { skip: skipDb }, async () => {
  const guild = await dbGuild(54, { captchaChannelId: 'CA' });
  guild.members.me.roles.cache.set('BR', mkRole('BR', { permissions: perms('ViewChannel') }));
  const role = guild.roles.cache.get('RV');
  role.setPermissions = async (bits) => { role.permissions = new PermissionsBitField(bits); };
  guild.roles.everyone.setPermissions = async (bits) => { guild.roles.everyone.permissions = new PermissionsBitField(bits); };
  const channel = mkChannel(guild, 'CA');
  const admin = mkMember(guild, UID(540), { permissions: perms('Administrator') });
  const stored = async () => (await prisma.guildSettings.findUnique({ where: { guildId: guild.id } })).captchaLockdownPrior;

  // Cancelled: nothing changes.
  const cancelled = confirming(mkSlash(guild, admin, 'lock', { state: 'on' }), 'no');
  await cmd.execute(cancelled);
  assert.equal(isLocked(guild), false);
  assert.equal(await stored(), null);

  const on = confirming(mkSlash(guild, admin, 'lock', { state: 'on' }));
  await cmd.execute(on);
  assert.match(embedText(on.log.replies[0].embeds[0].toJSON()), /Give the role to members already here first/);
  assert.match(embedText(lastEmbed(on.log.edits)), /Server Locked/);
  assert.equal(isLocked(guild), true);
  assert.equal(role.permissions.has(PermissionFlagsBits.ViewChannel), true);
  assert.equal(channel.permissionOverwrites.edited.length, 1);
  assert.equal(await stored(), true); // @everyone could view channels before the lock

  // A second "on" has nothing to do.
  const again = mkSlash(guild, admin, 'lock', { state: 'on' });
  await cmd.execute(again);
  assert.match(embedText(lastEmbed(again.log.replies)), /Already Locked/);

  // Off: the stored state was "open", so the warning about private servers is not shown.
  const off = confirming(mkSlash(guild, admin, 'lock', { state: 'off' }));
  await cmd.execute(off);
  assert.doesNotMatch(embedText(off.log.replies[0].embeds[0].toJSON()), /do not unlock/);
  assert.match(embedText(lastEmbed(off.log.edits)), /Server Unlocked/);
  assert.equal(isLocked(guild), false);
  assert.equal(await stored(), null);
});
