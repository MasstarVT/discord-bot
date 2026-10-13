// pkg-moderation tests (pkg-moderation.md §9): pure helpers, /modconfig payload, plugin declaration, static scan,
// reply helpers, and (with TEST_DATABASE_URL) the DB cases: case numbering under the advisory lock, ban import,
// expiry reconciliation, job handlers, executeModAction / executeUnban end to end, escalation and /modconfig writes.
// Offline by default. DB cases: TEST_DATABASE_URL='<migrated db>' node --test --test-concurrency=1 test/moderation.test.js
import { test, after } from 'node:test';
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

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const svc = await import('../src/services/moderationService.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { updateSettings, getSettings } = await import('../src/services/settingsService.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const modconfig = await import('../src/commands/moderation/modconfig.js');

const {
  ACTION_META, MSG, LEGACY_ESCALATION_SINCE, OVERDUE_BAN_LIFT_LIMIT_MS,
  resolveEffectiveAction, normalizeEscalationTiers, selectEscalationTier, describeTier, preflightModAction, isImmune,
  mapDiscordError, buildAuditReason, buildModLogEmbed, displayType, maxSnowflake, markBotBanAction, wasBotBanAction,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const DAY = 86_400_000;
const HOUR = 3_600_000;

// ── 1. resolveEffectiveAction ────────────────────────────────────────────────

test('resolveEffectiveAction', () => {
  const r = resolveEffectiveAction;
  assert.deepEqual(r({ action: 'TIMEOUT', durationMs: 10 * 60_000 }), { ok: true, action: 'TIMEOUT', durationMs: 600_000, note: null });
  const long = r({ action: 'TIMEOUT', durationMs: 30 * DAY, muteRoleId: '1' });
  assert.equal(long.ok, true);
  assert.equal(long.action, 'MUTE');
  assert.match(long.note, /mute role was used/);
  const noRole = r({ action: 'TIMEOUT', durationMs: 30 * DAY });
  assert.equal(noRole.ok, false);
  assert.equal(noRole.code, 'INVALID');
  assert.equal(r({ action: 'TIMEOUT' }).ok, false);
  assert.equal(r({ action: 'TIMEOUT', durationMs: 30_000 }).ok, false);
  assert.equal(r({ action: 'TIMEOUT', durationMs: -5 }).error, 'Invalid duration.');
  assert.equal(r({ action: 'TIMEOUT', durationMs: NaN }).error, 'Invalid duration.');
  assert.equal(r({ action: 'TIMEOUT', durationMs: 400 * DAY, muteRoleId: '1' }).error, 'Mutes can last at most 1 year.');
  const mute1d = r({ action: 'MUTE', durationMs: DAY });
  assert.equal(mute1d.action, 'TIMEOUT');
  assert.match(mute1d.note, /No mute role is set/);
  assert.equal(r({ action: 'MUTE' }).ok, false);
  assert.equal(r({ action: 'MUTE', durationMs: 40 * DAY }).ok, false);
  assert.deepEqual(r({ action: 'MUTE', muteRoleId: '1' }), { ok: true, action: 'MUTE', durationMs: null, note: null });
  assert.equal(r({ action: 'BAN', durationMs: DAY }).action, 'TEMPBAN');
  assert.equal(r({ action: 'BAN' }).action, 'BAN');
  assert.equal(r({ action: 'TEMPBAN', durationMs: 2 * 365 * DAY }).ok, false);
  assert.equal(r({ action: 'TEMPBAN' }).ok, false);
  assert.equal(r({ action: 'TEMPBAN', durationMs: DAY }).durationMs, DAY);
  assert.equal(r({ action: 'KICK', durationMs: DAY }).durationMs, null);
  assert.equal(r({ action: 'WARN', durationMs: DAY }).durationMs, null);
  assert.equal(r({ action: 'SOFTBAN', durationMs: DAY }).durationMs, null);
  const foo = r({ action: 'FOO' });
  assert.equal(foo.ok, false);
  assert.equal(foo.code, 'INVALID');
});

// ── 2. normalizeEscalationTiers ──────────────────────────────────────────────

test('normalizeEscalationTiers: legacy rows, drops, clamps, ids, order, cap', () => {
  const n = normalizeEscalationTiers;
  const legacy = n([{ warnCount: 3, action: 'TIMEOUT', duration: 600 }, { warnCount: 5, action: 'BAN', duration: 86400 }]);
  assert.deepEqual(legacy.map((t) => [t.id, t.warnCount, t.withinSeconds, t.action, t.durationSeconds]), [
    [1, 3, null, 'TIMEOUT', 600],
    [2, 5, null, 'TEMPBAN', 86400],
  ]);
  assert.ok(legacy.every((t) => t.countFrom === LEGACY_ESCALATION_SINCE), 'legacy tiers without countFrom get the fallback');

  assert.equal(n([{ warnCount: 3, action: 'BAN' }])[0].action, 'BAN', 'a legacy BAN without a duration stays permanent');
  assert.deepEqual(n([
    { warnCount: 3, action: 'TIMEOUT' },
    { warnCount: 0, action: 'KICK' },
    { warnCount: 101, action: 'KICK' },
    { warnCount: 2, action: 'EXPLODE' },
    null, 'x', 5, [],
  ]), []);
  assert.equal(n([{ warnCount: 2, action: 'timeout', duration: 5_000_000 }])[0].durationSeconds, 2_419_200);
  assert.equal(n([{ warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: 99 }])[0].durationSeconds, null);
  assert.equal(n([{ warnCount: 2, withinSeconds: null, action: 'MUTE', durationSeconds: null }])[0].durationSeconds, null);
  assert.equal(n([{ warnCount: 2, withinSeconds: null, action: 'TEMPBAN', durationSeconds: 5 }])[0].durationSeconds, 60);

  const dup = n([
    { id: 1, warnCount: 2, withinSeconds: 3600, action: 'KICK', durationSeconds: null },
    { id: 2, warnCount: 2, withinSeconds: 3600, action: 'BAN', durationSeconds: null },
  ]);
  assert.equal(dup.length, 1);
  assert.equal(dup[0].action, 'BAN', 'the last duplicate wins');

  const sorted = n([
    { id: 1, warnCount: 5, withinSeconds: null, action: 'KICK', durationSeconds: null },
    { id: 2, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null },
    { id: 3, warnCount: 2, withinSeconds: 86400, action: 'KICK', durationSeconds: null },
    { id: 4, warnCount: 2, withinSeconds: 3600, action: 'KICK', durationSeconds: null },
  ]);
  assert.deepEqual(sorted.map((t) => t.id), [4, 3, 2, 1]);

  const many = n(Array.from({ length: 12 }, (_, i) => ({ warnCount: i + 1, withinSeconds: null, action: 'KICK', durationSeconds: null })));
  assert.equal(many.length, LIMITS.ESCALATION_TIERS);

  const kept = n([
    { id: 7, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null },
    { id: 7, warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null },
    { warnCount: 4, withinSeconds: null, action: 'KICK', durationSeconds: null },
  ]);
  assert.deepEqual(kept.map((t) => t.id), [7, 8, 9], 'ids are preserved and duplicates / missing ones are reassigned above the maximum');

  for (const bad of [null, {}, 'x', 42, undefined]) assert.deepEqual(n(bad), []);
});

test('normalizeEscalationTiers keeps countFrom (valid) and nulls it (invalid) on v2 tiers', () => {
  const [a, b] = normalizeEscalationTiers([
    { id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: '2026-01-02T00:00:00.000Z' },
    { id: 2, warnCount: 3, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: 'not a date' },
  ]);
  assert.equal(a.countFrom, '2026-01-02T00:00:00.000Z');
  assert.equal(b.countFrom, null, 'a v2 tier with an invalid countFrom counts all active warnings');
  const [legacy] = normalizeEscalationTiers([{ warnCount: 3, action: 'KICK', countFrom: '2026-05-05T05:05:05.000Z' }]);
  assert.equal(legacy.countFrom, '2026-05-05T05:05:05.000Z', 'the migration stamp is kept on a legacy-shaped tier');
});

// ── 3. selectEscalationTier ──────────────────────────────────────────────────

const NOW = 1_000_000_000_000;
const TIERS = normalizeEscalationTiers([
  { id: 1, warnCount: 2, withinSeconds: 86400, action: 'TIMEOUT', durationSeconds: 86400, countFrom: null },
  { id: 2, warnCount: 5, withinSeconds: 604800, action: 'TEMPBAN', durationSeconds: 259200, countFrom: null },
  { id: 3, warnCount: 10, withinSeconds: 1_209_600, action: 'BAN', durationSeconds: null, countFrom: null },
]);
const ago = (ms) => NOW - ms;

test('selectEscalationTier: windows, severity, once per window, fallback', () => {
  const pick = (warns, prior = []) => selectEscalationTier(TIERS, warns, prior, NOW)?.tier.id ?? null;
  assert.equal(pick([ago(1000), ago(HOUR)]), 1, '2 warns in the last hour -> A');
  assert.equal(pick([ago(1000), ago(HOUR)], [{ tierId: 1, at: NOW - 1000 }]), null, 'A already fired in its window');
  assert.equal(pick([ago(3 * DAY), ago(2 * DAY), ago(DAY + HOUR), ago(2 * HOUR), ago(HOUR)]), 2, '5 within 3 days -> B (most severe)');
  assert.equal(
    pick([ago(3 * DAY), ago(2 * DAY), ago(DAY + HOUR), ago(2 * HOUR), ago(HOUR)], [{ tierId: 2, at: NOW - DAY }]),
    1,
    'B fired yesterday, 2 warns in the last 12 h -> fall back to A',
  );
  assert.equal(pick(Array.from({ length: 10 }, (_, i) => ago((i + 1) * DAY))), 3, '10 within 10 days -> C');
  assert.equal(pick([ago(1000)]), null, 'one warn');
  assert.equal(pick([]), null);
  assert.equal(selectEscalationTier(TIERS, [ago(1), ago(2)], [], NOW).count, 2);
});

test('selectEscalationTier: a null window starts at the oldest active warn (a pardon restarts it)', () => {
  const tiers = normalizeEscalationTiers([{ id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null }]);
  const warns = [NOW - 5 * DAY, NOW - 4 * DAY];
  assert.equal(selectEscalationTier(tiers, warns, [{ tierId: 1, at: NOW - 10 * DAY }], NOW)?.tier.id, 1, 'an escalation before the oldest warn does not block');
  assert.equal(selectEscalationTier(tiers, warns, [{ tierId: 1, at: NOW - 4.5 * DAY }], NOW), null, 'an escalation after the oldest warn blocks');
});

test('selectEscalationTier: countFrom hides older warns from a tier', () => {
  const countFrom = new Date(NOW - HOUR).toISOString();
  const tiers = normalizeEscalationTiers([{ id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom }]);
  const old5 = Array.from({ length: 5 }, (_, i) => NOW - (2 + i) * HOUR);
  assert.equal(selectEscalationTier(tiers, [...old5, NOW - 1000], [], NOW), null, '5 old warns + 1 new: only 1 counts');
  assert.equal(selectEscalationTier(tiers, [...old5, NOW - 1000, NOW - 2000], [], NOW)?.tier.id, 1, '2 new warns count');
  const windowed = normalizeEscalationTiers([{ id: 1, warnCount: 2, withinSeconds: 86400, action: 'KICK', durationSeconds: null, countFrom }]);
  assert.equal(selectEscalationTier(windowed, [...old5, NOW - 1000], [], NOW), null, 'the window start is max(window, countFrom)');
});

// ── 4. describeTier ──────────────────────────────────────────────────────────

test('describeTier', () => {
  const text = describeTier({ id: 3, warnCount: 5, withinSeconds: 604800, action: 'TEMPBAN', durationSeconds: 259200 });
  assert.equal(text, 'T3: 5 warnings within 1 week → Temporary ban for 3 days'); // wording follows formatDurationLong
  assert.ok(text.length <= 100);
  assert.equal(describeTier({ id: 1, warnCount: 1, withinSeconds: null, action: 'KICK', durationSeconds: null }), 'T1: 1 warning → Kick');
  assert.match(describeTier({ id: 2, warnCount: 2, withinSeconds: null, action: 'MUTE', durationSeconds: null }), /until unmuted$/);
  assert.ok(describeTier({ id: 99, warnCount: 100, withinSeconds: 31_535_999, action: 'TEMPBAN', durationSeconds: 31_535_999 }).length <= 100);
});

// ── 5. preflightModAction ────────────────────────────────────────────────────

const BOT = '100000000000000001';
const OWNER = '100000000000000002';
const ALICE = '100000000000000003';
const BOB = '100000000000000004';
const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));

function mkRole(id, position = 1, extra = {}) {
  return { id, position, managed: false, mentionable: false, permissions: new PermissionsBitField(), ...extra };
}

function mkMember(guild, id, { position = 1, perms = [], roles = [], user = {}, ...flags } = {}) {
  const roleMap = new Map(roles.map((r) => [r.id, r]));
  const member = {
    id,
    guild,
    user: { id, username: `user${id.slice(-2)}`, tag: `user${id.slice(-2)}`, bot: false, send: async () => ({ delete: async () => {} }), displayAvatarURL: () => 'https://cdn.example/a.png', ...user },
    permissions: bits(...perms),
    roles: {
      cache: roleMap,
      highest: { position, comparePositionTo(other) { return this.position - other.position; } },
      add: async () => {},
      remove: async () => {},
    },
    moderatable: true,
    kickable: true,
    bannable: true,
    manageable: true,
    isCommunicationDisabled: () => false,
    timeout: async () => {},
    kick: async () => {},
    ...flags,
  };
  return member;
}

function mkGuild(id = '9550030000000000') {
  const guild = { id, ownerId: OWNER, available: true, name: 'Test Guild' };
  guild.members = { cache: new Map(), me: null, fetchMe: async () => guild.members.me, fetch: async (uid) => { const m = guild.members.cache.get(uid); if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 }); return m; } };
  guild.roles = { cache: new Map(), fetch: async () => new Map(guild.roles.cache) };
  guild.channels = { cache: new Map(), fetch: async () => null };
  guild.client = {
    user: { id: BOT, username: 'Bot', tag: 'Bot#0' },
    users: { fetch: async (uid) => ({ id: uid, username: `u${uid.slice(-3)}`, tag: `u${uid.slice(-3)}`, send: async () => ({ delete: async () => {} }) }) },
  };
  guild.bans = {
    fetch: async () => { throw Object.assign(new Error('Unknown Ban'), { code: 10026 }); },
    create: async () => {},
    remove: async () => {},
  };
  const me = mkMember(guild, BOT, { position: 100, perms: ['BanMembers', 'KickMembers', 'ModerateMembers', 'ManageRoles', 'ViewAuditLog'] });
  guild.members.me = me;
  guild.members.cache.set(BOT, me);
  return guild;
}

const base = (guild, over = {}) => ({ action: 'KICK', guild, me: guild.members.me, member: null, targetId: ALICE, executor: null, settings: null, source: 'command', muteRole: null, ...over });

test('preflightModAction', () => {
  const guild = mkGuild();
  const me = guild.members.me;
  const alice = mkMember(guild, ALICE, { position: 5 });
  const execMod = mkMember(guild, BOB, { position: 10, perms: ['KickMembers'] });
  const execOwner = mkMember(guild, OWNER, { position: 1 });
  const pre = (over) => preflightModAction(base(guild, { member: alice, executor: execMod, ...over }));

  assert.deepEqual(pre({ targetId: BOT }), { code: 'HIERARCHY', error: MSG.BOT_SELF });
  assert.deepEqual(pre({ targetId: OWNER }), { code: 'HIERARCHY', error: MSG.OWNER });
  assert.deepEqual(pre({ executor: alice }), { code: 'INVALID', error: MSG.SELF });
  assert.equal(pre({ member: null }).code, 'NOT_IN_GUILD');
  assert.equal(pre({ action: 'WARN', member: null }).code, 'NOT_IN_GUILD');
  assert.equal(pre({ action: 'BAN', member: null }), null, 'a ban by id needs no member');

  const noBan = { ...me, permissions: bits('KickMembers') };
  const e = pre({ action: 'BAN', me: noBan });
  assert.equal(e.code, 'NOT_PERMITTED');
  assert.match(e.error, /Ban Members/);

  const immuneRole = mkRole('900000000000000001');
  const immuneMember = mkMember(guild, ALICE, { position: 5, roles: [immuneRole] });
  const settings = { immuneRoleIds: [immuneRole.id], modRoleIds: [] };
  assert.equal(pre({ member: immuneMember, settings, source: 'automod', executor: null }).code, 'IMMUNE');
  assert.equal(pre({ member: immuneMember, settings }).error, MSG.IMMUNE_ROLE, 'a command on an immune role member is refused');
  assert.equal(pre({ member: immuneMember, settings, executor: execOwner }), null, 'the guild owner bypasses the immunity check');

  const admin = mkMember(guild, ALICE, { position: 5, perms: ['Administrator'] });
  assert.equal(pre({ member: admin }).error, MSG.IMMUNE_ADMIN, 'moderator -> administrator');
  assert.equal(pre({ member: admin, executor: mkMember(guild, BOB, { position: 10, perms: ['Administrator'] }) }).error, MSG.IMMUNE_ADMIN, 'admin -> admin');
  const prev = process.env.BOT_OWNER_ID;
  process.env.BOT_OWNER_ID = BOB;
  assert.equal(pre({ member: admin, action: 'KICK' }), null, 'BOT_OWNER_ID (OWNER level) -> admin is allowed');
  process.env.BOT_OWNER_ID = prev ?? '';
  if (prev === undefined) delete process.env.BOT_OWNER_ID;

  assert.equal(pre({ action: 'TIMEOUT', member: admin, executor: execOwner }).error, MSG.ADMIN_TIMEOUT);
  assert.equal(pre({ action: 'KICK', member: mkMember(guild, ALICE, { kickable: false }) }).error, MSG.BOT_ROLE);
  assert.equal(pre({ action: 'TIMEOUT', member: mkMember(guild, ALICE, { moderatable: false }) }).error, MSG.BOT_ROLE);
  assert.equal(pre({ action: 'BAN', member: mkMember(guild, ALICE, { bannable: false }) }).error, MSG.BOT_ROLE);

  assert.equal(pre({ executor: mkMember(guild, BOB, { position: 5, perms: ['KickMembers'] }) }).error, MSG.EXEC_ROLE, 'equal roles are refused');
  assert.equal(pre({ executor: mkMember(guild, BOB, { position: 3, perms: ['KickMembers'] }) }).error, MSG.EXEC_ROLE);
  assert.equal(pre({ executor: execOwner }), null, 'owner executor with a lower role is allowed');
  assert.equal(pre({ action: 'WARN', member: mkMember(guild, ALICE, { position: 150, manageable: false, kickable: false }), executor: mkMember(guild, BOB, { position: 200, perms: ['ModerateMembers'] }) }), null, 'WARN has no bot-side role check');

  // MUTE: role checks
  assert.equal(pre({ action: 'MUTE', muteRole: null }).error, MSG.MUTE_ROLE_GONE);
  const high = mkRole('900000000000000002', 150);
  assert.match(pre({ action: 'MUTE', muteRole: high }).error, /^Mute role problem: .*above my highest role/);
  assert.equal(pre({ action: 'MUTE', muteRole: mkRole('900000000000000003', 5) }), null);
});

// ── 6. isImmune ──────────────────────────────────────────────────────────────

test('isImmune', () => {
  const guild = mkGuild();
  const settings = { immuneRoleIds: ['900000000000000009'], modRoleIds: [] };
  assert.equal(isImmune(mkMember(guild, OWNER), settings), true, 'owner');
  assert.equal(isImmune(mkMember(guild, ALICE, { perms: ['Administrator'] }), settings), true, 'administrator');
  assert.equal(isImmune(mkMember(guild, ALICE, { roles: [mkRole('900000000000000009')] }), settings), true, 'immune role');
  assert.equal(isImmune(mkMember(guild, ALICE), settings), false, 'plain member');
  assert.equal(isImmune(null, settings), false);
  assert.equal(isImmune(mkMember(guild, ALICE), null), false);
  const throwing = mkMember(guild, ALICE);
  throwing.permissions = { has() { throw new Error('boom'); } };
  assert.equal(isImmune(throwing, settings), false);
});

// ── 7-8. mapDiscordError / buildAuditReason ──────────────────────────────────

test('mapDiscordError', () => {
  assert.equal(mapDiscordError({ code: 50013 }).code, 'NOT_PERMITTED');
  assert.equal(mapDiscordError({ code: 50001 }).code, 'NOT_PERMITTED');
  assert.equal(mapDiscordError({ code: 10007 }).code, 'NOT_IN_GUILD');
  assert.equal(mapDiscordError({ code: 10026 }).code, 'ALREADY');
  assert.equal(mapDiscordError({ code: 10011 }).code, 'INVALID');
  assert.equal(mapDiscordError({ code: 10013 }).error, 'Unknown user.');
  assert.match(mapDiscordError({ code: 50035, message: 'bad' }).error, /rejected the request: bad/);
  assert.equal(mapDiscordError(new Error('x')).code, 'DISCORD_ERROR');
  assert.equal(mapDiscordError(undefined).code, 'DISCORD_ERROR');
});

test('buildAuditReason', () => {
  assert.ok(buildAuditReason('x'.repeat(600), { username: 'mod' }, 'command').length <= 512);
  assert.match(buildAuditReason('spam', { username: 'mod' }, 'command'), /spam \| by mod$/);
  assert.ok(buildAuditReason('spam', { id: '1' }, 'automod').startsWith('[Automod]'));
  assert.ok(buildAuditReason('[Escalation T1] Reached 2', { id: '1' }, 'escalation').startsWith('[Escalation T1]'));
  assert.ok(buildAuditReason('r', { id: '1' }, 'captcha').startsWith('[Captcha]'));
});

// ── 9. buildModLogEmbed ──────────────────────────────────────────────────────

test('buildModLogEmbed', () => {
  const infraction = {
    caseId: 7, targetUserId: ALICE, moderatorId: BOB, type: 'BAN', reason: 'r'.repeat(3000),
    proofUrl: 'https://example.com/' + 'p'.repeat(2000), expiresAt: new Date(Date.now() + DAY), createdAt: new Date(),
  };
  const embed = buildModLogEmbed({
    infraction, target: { tag: 'a`b', displayAvatarURL: () => 'https://cdn.example/a.png' }, moderator: { username: 'mod' },
    source: 'automod', logFields: [1, 2, 3, 4, 5].map((i) => ({ name: `n${i}`, value: 'v'.repeat(2000) })),
  });
  const json = embed.toJSON();
  assert.ok(json.title.includes('Temporary ban'));
  const reason = json.fields.find((f) => f.name === 'Reason');
  assert.ok(reason.value.length <= 1024);
  assert.ok(json.fields.find((f) => f.name === 'Source'));
  assert.ok(json.fields.find((f) => f.name === 'Expires'));
  assert.ok(json.fields.find((f) => f.name === 'Proof').value.length <= 1024);
  assert.equal(json.fields.filter((f) => /^n\d$/.test(f.name)).length, 3, 'logFields beyond 3 are ignored');
  assert.ok(json.fields.every((f) => f.value.length <= 1024));
  assert.equal(json.footer.text, `User ID: ${ALICE}`);
  assert.doesNotThrow(() => buildModLogEmbed({ infraction: { ...infraction, reason: '', proofUrl: null, expiresAt: null, type: 'WARN' }, target: { id: ALICE }, moderator: null }).toJSON());
  const plain = buildModLogEmbed({ infraction: { ...infraction, expiresAt: null, type: 'WARN' }, target: { id: ALICE }, moderator: null }).toJSON();
  assert.equal(plain.fields.find((f) => f.name === 'Source'), undefined, 'no Source field for commands');
});

// ── 10-12. constants and markers ─────────────────────────────────────────────

test('ACTION_META and displayType', () => {
  assert.equal(ACTION_META.WARN.active, true);
  assert.equal(displayType({ type: 'BAN', expiresAt: new Date() }), 'TEMPBAN');
  assert.equal(displayType({ type: 'BAN', expiresAt: null }), 'BAN');
  assert.equal(displayType({ type: 'WARN', expiresAt: new Date() }), 'WARN');
  assert.equal(displayType(null), undefined);
  assert.equal(OVERDUE_BAN_LIFT_LIMIT_MS, 30 * DAY);
});

test('maxSnowflake compares as BigInt', () => {
  assert.equal(maxSnowflake(['9', '10', '100000000000000001']), '100000000000000001');
  assert.equal(maxSnowflake([]), null);
  assert.equal(maxSnowflake(['abc', '5']), '5');
});

test('ban markers', async () => {
  markBotBanAction('g1', 'u1', 'ban');
  assert.equal(wasBotBanAction('g1', 'u1', 'ban'), true);
  assert.equal(wasBotBanAction('g1', 'u1', 'ban'), true, 'non-destructive');
  assert.equal(wasBotBanAction('g1', 'u1', 'unban'), false, 'kinds are separate');
  markBotBanAction('g1', 'u2', 'unban', 1);
  await new Promise((r) => setTimeout(r, 15));
  assert.equal(wasBotBanAction('g1', 'u2', 'unban'), false, 'expired');
});

// ── 13. /modconfig payload ───────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/modconfig payload', () => {
  const json = buildCommandPayload(modconfig);
  assert.equal(json.name, 'modconfig');
  assert.ok(charCount(json) < 4000, `charCount ${charCount(json)}`);
  assert.equal(json.options.length, 8);
  assert.deepEqual(json.options.map((o) => o.name), ['escalation', 'immune', 'modroles', 'muterole', 'logchannel', 'visibility', 'dm', 'show']);
  assert.equal(modconfig.gated, false);
  assert.equal(modconfig.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(modconfig.module, 'moderation');
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.equal(typeof modconfig.autocomplete, 'function');
  const remove = json.options[0].options.find((o) => o.name === 'remove').options[0];
  assert.equal(remove.autocomplete, true);
  assert.equal(json.options[0].options.find((o) => o.name === 'add').options.length, 4);
});

// ── 14. plugin ───────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /moderation/.test(e)), []);
  const p = plugins.find((x) => x.id === 'moderation');
  assert.ok(p, 'collectPlugins returns the moderation plugin');
  assert.equal(p.module, 'moderation');
  assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}`).sort(), [
    'guildBanAdd:moderation', 'guildBanRemove:moderation', 'guildMemberAdd:moderation', 'guildReady:moderation',
  ]);
  assert.deepEqual(p.jobs.map((j) => j.type).sort(), ['mod.expire', 'mod.unban']);
  assert.equal(p.components.length, 0);
  const hook = (ev) => p.hooks.find((h) => h.event === ev);
  assert.equal(hook('guildReady').always, true);
  assert.equal(hook('guildBanRemove').always, true);
  assert.equal(hook('guildBanAdd').always, undefined, 'recording external bans is gated by the module');
  assert.equal(hook('guildMemberAdd').filter({ member: { user: { bot: true } } }), false, 'bots are skipped');
  assert.equal(hook('guildMemberAdd').filter({ member: { user: { bot: false } } }), true);
  assert.equal(hook('guildBanAdd').filter({ user: { id: BOT }, client: { user: { id: BOT } } }), false, 'our own bans are skipped');
});

// ── 15. static scan ──────────────────────────────────────────────────────────

test('static scan of the moderation-owned source files', () => {
  const files = ['src/services/moderationService.js', 'src/plugins/moderation.js', 'src/commands/moderation/modconfig.js'];
  for (const f of files) {
    const text = readFileSync(resolve(ROOT, f), 'utf8');
    assert.doesNotMatch(text, /setCustomId\(|customId\s*:/, `${f}: customId literal`);
    assert.doesNotMatch(text, /\bconsole\./, `${f}: console`);
    assert.doesNotMatch(text, /cache\.del\(/, `${f}: cache.del`);
    assert.doesNotMatch(text, /deleteMessageDays/, `${f}: deleteMessageDays`);
    assert.doesNotMatch(text, /roles\.add\(\[/, `${f}: array-form roles.add`);
    assert.doesNotMatch(text, /LIMITS\.[A-Z_]+\s*\?\?/, `${f}: LIMITS fallback`);
  }
  const service = readFileSync(resolve(ROOT, 'src/services/moderationService.js'), 'utf8');
  assert.match(service, /pg_advisory_xact_lock/);
  for (const m of service.matchAll(/(?:\$queryRaw|\$executeRaw|Prisma\.sql)\s*`([\s\S]*?)`\)?/g)) {
    assert.ok(!/(?<![\w.])now\(\)|CURRENT_TIMESTAMP/i.test(m[1]), 'no bare now() in raw SQL');
  }
  // Every `_max: { caseId` aggregate sits inside recordInfraction or recordInfractionsBulk.
  const bounds = ['export async function recordInfraction(', 'export async function recordInfractionsBulk(', 'export async function resolveInfractions('].map((s) => service.indexOf(s));
  assert.ok(bounds.every((i) => i > 0));
  for (const m of service.matchAll(/_max:\s*\{\s*caseId/g)) {
    assert.ok(m.index > bounds[0] && m.index < bounds[2], '_max.caseId outside the allocation functions');
  }
  assert.ok(Array.isArray(svc.RAW_SQL) && svc.RAW_SQL.every((r) => typeof r.build === 'function'));
});

// ── Reply helpers (fake interactions) ────────────────────────────────────────

function fakeChannel(usable = true) {
  const sent = [];
  const guild = { members: { me: {} } };
  return {
    id: '800000000000000001', guild, sent,
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => (usable ? bits('ViewChannel', 'SendMessages', 'EmbedLinks') : bits('ViewChannel')),
    send: async (p) => { sent.push(p); },
  };
}

function fakeReplyInteraction(channel) {
  const calls = [];
  return {
    calls, channel,
    deleteReply: async () => { calls.push(['deleteReply']); },
    followUp: async (p) => { calls.push(['followUp', p]); },
    editReply: async (p) => { calls.push(['editReply', p]); },
  };
}

test('deliverModReply honours modVisibility', async () => {
  const reply = { title: 'User Banned', lines: ['**User:** x', '**Case:** #1'], warnings: ['careful'] };

  const pub = fakeReplyInteraction(fakeChannel());
  await svc.deliverModReply(pub, { modVisibility: 'PUBLIC' }, reply);
  assert.deepEqual(pub.calls.map((c) => c[0]), ['deleteReply', 'followUp', 'followUp']);
  assert.equal(pub.calls[2][1].ephemeral, true, 'warnings go out privately');
  assert.deepEqual(pub.calls[1][1].allowedMentions, { parse: [] });

  const priv = fakeReplyInteraction(fakeChannel());
  await svc.deliverModReply(priv, { modVisibility: 'PRIVATE' }, reply);
  assert.deepEqual(priv.calls.map((c) => c[0]), ['editReply']);
  assert.match(priv.calls[0][1].embeds[0].toJSON().description, /careful/);

  const channel = fakeChannel();
  const anon = fakeReplyInteraction(channel);
  await svc.deliverModReply(anon, { modVisibility: 'ANONYMOUS' }, reply);
  assert.equal(channel.sent.length, 1, 'the public notice is a channel message (no invoker shown)');
  assert.deepEqual(anon.calls.map((c) => c[0]), ['editReply']);
  assert.match(anon.calls[0][1].embeds[0].toJSON().description, /without your name/);

  const blocked = fakeReplyInteraction(fakeChannel(false));
  await svc.deliverModReply(blocked, { modVisibility: 'ANONYMOUS' }, reply);
  assert.match(blocked.calls[0][1].embeds[0].toJSON().description, /missing permissions/);

  const failing = fakeChannel();
  failing.send = async () => { throw new Error('nope'); };
  const failedSend = fakeReplyInteraction(failing);
  await svc.deliverModReply(failedSend, { modVisibility: 'ANONYMOUS' }, reply);
  assert.match(failedSend.calls[0][1].embeds[0].toJSON().description, /couldn't post the public notice/);

  const broken = { deleteReply: async () => { throw new Error('x'); }, followUp: async () => { throw Object.assign(new Error('gone'), { code: 10062 }); }, editReply: async () => {} };
  await assert.doesNotReject(svc.deliverModReply(broken, null, reply));
});

test('deliverLookup and replyModError', async () => {
  const pub = fakeReplyInteraction(null);
  await svc.deliverLookup(pub, null, { content: 'x' });
  assert.deepEqual(pub.calls.map((c) => c[0]), ['deleteReply', 'followUp']);
  const priv = fakeReplyInteraction(null);
  await svc.deliverLookup(priv, { modVisibility: 'ANONYMOUS' }, { content: 'x' });
  assert.deepEqual(priv.calls.map((c) => c[0]), ['editReply']);

  const err = fakeReplyInteraction(null);
  await svc.replyModError(err, { code: 'HIERARCHY', error: 'no' });
  assert.match(err.calls[0][1].embeds[0].toJSON().title, /Not Allowed/);
  await svc.replyModError(err, { code: 'DISCORD_ERROR', error: 'saved?', applied: true });
  assert.match(err.calls[1][1].embeds[0].toJSON().title, /Done, But Not Recorded/);
});

// ── executeModAction input validation (offline: fails before any settings load) ──

test('executeModAction validates its input before touching anything', async () => {
  const guild = mkGuild();
  const run = (over) => svc.executeModAction({ guild, target: { id: ALICE }, moderator: guild.client.user, action: 'WARN', source: 'automod', ...over });
  assert.equal((await svc.executeModAction()).code, 'INVALID');
  assert.equal((await run({ target: { id: 'abc' } })).code, 'INVALID');
  assert.equal((await run({ moderator: null })).code, 'INVALID');
  assert.equal((await run({ source: 'automation' })).code, 'INVALID', "'automation' is no longer a source");
  assert.equal((await run({ proofUrl: 'ftp://x' })).error, MSG.BAD_PROOF);
  assert.equal((await run({ deleteMessageSeconds: 700_000 })).code, 'INVALID');
  assert.equal((await run({ deleteMessageSeconds: 1.5 })).code, 'INVALID');
});

// ═════════════════════════════════════════════════════════════════════════════
// DB cases (TEST_DATABASE_URL): guild ids '9550030…' belong to this package
// ═════════════════════════════════════════════════════════════════════════════

const G = (n) => `95500300000000${String(n).padStart(2, '0')}`;
const U = (n) => `55003000000000${String(n).padStart(5, '0')}`; // 19 digits
const TEST_GUILDS = Array.from({ length: 20 }, (_, i) => G(i + 1));

async function cleanup() {
  await prisma.scheduledJob.deleteMany({ where: { guildId: { in: TEST_GUILDS } } });
  await prisma.guildSettings.deleteMany({ where: { guildId: { in: TEST_GUILDS } } });
}
if (DB) {
  await cleanup();
  after(cleanup);
}

const jobOf = (key) => prisma.scheduledJob.findUnique({ where: { dedupeKey: key } });
const waitFor = async (fn, ms = 4000) => {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) return v;
    await new Promise((r) => setTimeout(r, 50));
  }
};

test('DB: RAW_SQL executes and case numbers are unique under concurrency (R153-R156)', { skip: skipDb }, async () => {
  await prisma.$transaction(async (tx) => {
    for (const { name, build } of svc.RAW_SQL) {
      try { await tx.$executeRaw(build()); } catch (err) { assert.fail(`${name}: ${err.message}`); }
    }
  });
  const guildId = G(1);
  const made = await Promise.all(Array.from({ length: 15 }, (_, i) => svc.recordInfraction({
    guildId, targetUserId: U(i), moderatorId: U(99), type: i % 2 ? 'WARN' : 'NOTE', reason: `r${i}`,
  })));
  assert.deepEqual(made.map((r) => r.caseId).sort((a, b) => a - b), Array.from({ length: 15 }, (_, i) => i + 1));
  assert.equal(made.find((r) => r.type === 'WARN').active, true, 'WARN rows start active (R041/R042)');
  assert.equal(made.find((r) => r.type === 'NOTE').active, false);

  const inf = await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'BAN', reason: ' '.repeat(3), proofUrl: 'https://x.test/p', expiresAt: new Date(Date.now() + DAY) });
  assert.equal(inf.caseId, 16);
  assert.equal(inf.reason, 'No reason provided');
  assert.equal(displayType(inf), 'TEMPBAN');
  const long = await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'WARN', reason: 'z'.repeat(5000) });
  assert.ok(long.reason.length <= 1000);

  const bulk = await svc.recordInfractionsBulk(guildId, Array.from({ length: 5 }, (_, i) => ({ targetUserId: U(200 + i), moderatorId: U(99), type: 'BAN', reason: `[Imported] b${i}`, active: true })));
  assert.equal(bulk, 5);
  const top = await prisma.infraction.aggregate({ where: { guildId }, _max: { caseId: true }, _count: true });
  assert.equal(top._max.caseId, 22);
  assert.equal(top._count, 22);
  const racing = await Promise.all([
    svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'NOTE' }),
    svc.recordInfractionsBulk(guildId, [{ targetUserId: U(300), moderatorId: U(99), type: 'BAN' }, { targetUserId: U(301), moderatorId: U(99), type: 'BAN' }]),
    svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'NOTE' }),
  ]);
  assert.equal(racing[1], 2);
  assert.equal((await prisma.infraction.aggregate({ where: { guildId }, _max: { caseId: true }, _count: true }))._count, 26);
  assert.equal((await prisma.infraction.aggregate({ where: { guildId }, _max: { caseId: true } }))._max.caseId, 26);
});

test('DB: resolveInfractions returns the number of rows closed', { skip: skipDb }, async () => {
  const guildId = G(2);
  for (let i = 0; i < 3; i++) await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'MUTE' });
  await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'BAN' });
  assert.equal(await svc.resolveInfractions(guildId, U(1), ['MUTE', 'TIMEOUT']), 3);
  assert.equal(await svc.resolveInfractions(guildId, U(1), ['MUTE', 'TIMEOUT']), 0);
  assert.equal(await svc.resolveInfractions(guildId, U(2), ['BAN']), 0);
  assert.equal(await svc.resolveInfractions(guildId, U(1), ['BAN']), 1);
});

test('DB: reconcileExpiries schedules, revives, skips ancient bans and closes expired timeouts', { skip: skipDb }, async () => {
  const guildId = G(3);
  const rec = (n, type, expiresAt, active = true) => svc.recordInfraction({ guildId, targetUserId: U(n), moderatorId: U(99), type, expiresAt, active });
  const future = await rec(1, 'BAN', new Date(Date.now() + DAY));
  const overdueOk = await rec(2, 'BAN', new Date(Date.now() - 2 * DAY));
  const ancient = await rec(3, 'BAN', new Date(Date.now() - 31 * DAY));
  const expiredTimeout = await rec(4, 'TIMEOUT', new Date(Date.now() - HOUR));
  const liveTimeout = await rec(5, 'TIMEOUT', new Date(Date.now() + HOUR));
  const liveMute = await rec(6, 'MUTE', new Date(Date.now() + HOUR));
  await rec(7, 'BAN', null); // permanent: nothing to schedule
  await rec(8, 'BAN', new Date(Date.now() + DAY), false); // closed: ignored
  const alreadyScheduled = await rec(9, 'BAN', new Date(Date.now() + 3 * DAY));
  const failed = await rec(10, 'BAN', new Date(Date.now() + 4 * DAY));
  // pre-existing jobs: a pending one (must be kept as is) and a FAILED one (must be revived)
  const kept = new Date(Date.now() + 5 * DAY);
  await prisma.scheduledJob.create({ data: { guildId, type: 'mod.unban', dedupeKey: `mod.unban:${guildId}:${U(9)}`, runAt: kept, payload: { userId: U(9), infractionId: alreadyScheduled.id } } });
  await prisma.scheduledJob.create({ data: { guildId, type: 'mod.unban', dedupeKey: `mod.unban:${guildId}:${U(10)}`, runAt: new Date(), status: 'FAILED', payload: { userId: U(10), infractionId: failed.id }, lastError: 'boom' } });

  const guild = { id: guildId };
  const out = await svc.reconcileExpiries(guild);
  assert.equal(out.expired, 1);
  assert.equal(out.overdue, 1, 'the ancient ban is counted, not scheduled');
  assert.equal(out.scheduled, 5, 'future ban, overdue ban, live timeout, live mute and the revived FAILED job (nothing for permanent, closed, ancient, kept)');

  const j1 = await jobOf(`mod.unban:${guildId}:${U(1)}`);
  assert.equal(j1.maxAttempts, 20);
  assert.equal(j1.payload.infractionId, future.id);
  assert.ok(await jobOf(`mod.unban:${guildId}:${U(2)}`), 'a ban overdue by <= 30 days is scheduled (runs on the next tick)');
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(3)}`), null, 'a ban overdue by > 30 days is not scheduled');
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(7)}`), null);
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(8)}`), null);
  assert.equal((await jobOf(`mod.unban:${guildId}:${U(9)}`)).runAt.getTime(), kept.getTime(), 'a pending job is left alone');
  const revived = await jobOf(`mod.unban:${guildId}:${U(10)}`);
  assert.equal(revived.status, 'PENDING');
  assert.equal(revived.maxAttempts, 20);
  assert.ok(await jobOf(`mod.expire:${liveTimeout.id}`));
  assert.ok(await jobOf(`mod.expire:${liveMute.id}`));
  assert.equal(await jobOf(`mod.expire:${expiredTimeout.id}`), null);
  assert.equal((await prisma.infraction.findUnique({ where: { id: expiredTimeout.id } })).active, false);
  assert.equal((await prisma.infraction.findUnique({ where: { id: overdueOk.id } })).active, true);
  assert.equal((await prisma.infraction.findUnique({ where: { id: ancient.id } })).active, true, 'the ancient ban stays active for /unban');
  assert.equal(await svc.countOverdueBans(guildId), 1);

  const again = await svc.reconcileExpiries(guild);
  assert.equal(again.scheduled, 0, 'a second pass finds everything scheduled');
});

test('DB: reconcileExpiries is not starved by 500+ ancient bans', { skip: skipDb }, async () => {
  const guildId = G(4);
  await svc.recordInfractionsBulk(guildId, Array.from({ length: 510 }, (_, i) => ({
    targetUserId: U(1000 + i), moderatorId: U(99), type: 'BAN', active: true, expiresAt: new Date(Date.now() - 60 * DAY),
  })));
  await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() + DAY) });
  const out = await svc.reconcileExpiries({ id: guildId });
  assert.equal(out.overdue, 510);
  assert.equal(out.scheduled, 1);
  assert.ok(await jobOf(`mod.unban:${guildId}:${U(1)}`));
});

test('DB: reapplyMuteOnJoin gives the mute role back only while a role mute is active', { skip: skipDb }, async () => {
  const guildId = G(5);
  const guild = mkGuild(guildId);
  const role = mkRole('900000000000000011', 5);
  guild.roles.cache.set(role.id, role);
  await updateSettings(guildId, { muteRoleId: role.id });
  const ctxFor = async (member) => ({ guild, member, getSettings: () => getSettings(guildId) });
  const added = [];
  const member = (id) => { const m = mkMember(guild, id); m.roles.add = async (r, why) => { added.push([id, r.id, why]); }; return m; };

  // permanent mute
  await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'MUTE' });
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(1))));
  assert.equal(added.length, 1);
  assert.match(added[0][2], /Re-applied mute after rejoin \(case #1\)/);
  // timed mute still running
  await svc.recordInfraction({ guildId, targetUserId: U(2), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() + HOUR) });
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(2))));
  assert.equal(added.length, 2);
  // expired but not yet swept
  await svc.recordInfraction({ guildId, targetUserId: U(3), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() - HOUR) });
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(3))));
  // inactive and unrelated
  await svc.recordInfraction({ guildId, targetUserId: U(4), moderatorId: U(99), type: 'MUTE', active: false });
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(4))));
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(5))));
  assert.equal(added.length, 2, 'expired, inactive and unknown members are left alone');
  // a role the bot cannot assign: never throws, nothing added
  role.position = 500;
  await assert.doesNotReject(svc.reapplyMuteOnJoin(await ctxFor(member(U(1)))));
  assert.equal(added.length, 2);
  role.position = 5;
  // no mute role configured
  await updateSettings(guildId, { muteRoleId: null });
  await svc.reapplyMuteOnJoin(await ctxFor(member(U(1))));
  assert.equal(added.length, 2);
  await assert.doesNotReject(svc.reapplyMuteOnJoin({}));
});

function jobFor(guildId, type, payload) {
  return { id: 'j', guildId, type, payload, runAt: new Date(), attempts: 1, maxAttempts: 20, dedupeKey: 'k', version: 0 };
}

test('DB: runUnbanJob lifts a temp ban once and backs off without permission', { skip: skipDb }, async () => {
  const guildId = G(6);
  const guild = mkGuild(guildId);
  const removed = [];
  guild.bans.remove = async (id, why) => { removed.push([id, why]); };
  const ctx = { client: guild.client, guild };
  const inf = await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() - 1000) });
  await scheduleLeftover(guildId, U(1));

  await assert.rejects(svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: 'bad' }), ctx), PermanentJobError);

  assert.equal(await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(1), infractionId: inf.id }), ctx), undefined);
  assert.equal(removed.length, 1);
  assert.match(removed[0][1], /Temporary ban expired \(case #1\)/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: inf.id } })).active, false);
  const unban = await prisma.infraction.findFirst({ where: { guildId, type: 'UNBAN' } });
  assert.ok(unban);
  assert.equal(unban.moderatorId, BOT);
  assert.equal(wasBotBanAction(guildId, U(1), 'unban'), true, 'the guildBanRemove hook will recognise the bot own unban');

  assert.equal(await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(1), infractionId: inf.id }), ctx), undefined);
  assert.equal(removed.length, 1, 'a second run is a no-op');
  assert.equal(await prisma.infraction.count({ where: { guildId, type: 'UNBAN' } }), 1);

  // missing permission: hourly back-off, then a permanent failure after 7 days
  const inf2 = await svc.recordInfraction({ guildId, targetUserId: U(2), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() - 1000) });
  const real = guild.members.me.permissions;
  guild.members.me.permissions = bits('KickMembers');
  const back = await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(2), infractionId: inf2.id }), ctx);
  assert.equal(back.payload.permRetries, 1);
  assert.ok(back.nextRunAt.getTime() - Date.now() > 3_500_000);
  await assert.rejects(svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(2), infractionId: inf2.id, permRetries: 168 }), ctx), PermanentJobError);
  guild.members.me.permissions = real;

  // Discord refuses at call time: same back-off; 10026 counts as success; anything else is retried by the scheduler
  guild.bans.remove = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  assert.equal((await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(2), infractionId: inf2.id }), ctx)).payload.permRetries, 1);
  guild.bans.remove = async () => { throw new Error('500'); };
  await assert.rejects(svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(2), infractionId: inf2.id }), ctx), /500/);
  guild.bans.remove = async () => { throw Object.assign(new Error('Unknown Ban'), { code: 10026 }); };
  await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(2), infractionId: inf2.id }), ctx);
  assert.equal((await prisma.infraction.findUnique({ where: { id: inf2.id } })).active, false);

  // an early run is deferred to the real expiry
  const inf3 = await svc.recordInfraction({ guildId, targetUserId: U(3), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() + DAY) });
  const early = await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(3), infractionId: inf3.id }), ctx);
  assert.equal(early.nextRunAt.getTime(), inf3.expiresAt.getTime());
  // a row of another guild is ignored
  const foreign = await svc.recordInfraction({ guildId: G(7), targetUserId: U(3), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() - 1000) });
  assert.equal(await svc.runUnbanJob(jobFor(guildId, 'mod.unban', { userId: U(3), infractionId: foreign.id }), ctx), undefined);
  assert.equal((await prisma.infraction.findUnique({ where: { id: foreign.id } })).active, true);
});

async function scheduleLeftover(guildId, userId) {
  await prisma.scheduledJob.create({ data: { guildId, type: 'mod.unban', dedupeKey: `mod.unban:${guildId}:${userId}`, runAt: new Date(), payload: { userId } } });
}

test('DB: runExpireJob closes timeouts and removes the mute role', { skip: skipDb }, async () => {
  const guildId = G(8);
  const guild = mkGuild(guildId);
  const role = mkRole('900000000000000021', 5);
  guild.roles.cache.set(role.id, role);
  await updateSettings(guildId, { muteRoleId: role.id });
  const removed = [];
  const member = mkMember(guild, U(2), { roles: [role] });
  member.roles.remove = async (rid, why) => { removed.push([rid, why]); };
  guild.members.cache.set(U(2), member);
  const ctx = { client: guild.client, guild };

  const timeout = await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'TIMEOUT', expiresAt: new Date(Date.now() - 1000) });
  assert.equal(await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: timeout.id }), ctx), undefined);
  assert.equal((await prisma.infraction.findUnique({ where: { id: timeout.id } })).active, false);
  assert.equal(await prisma.infraction.count({ where: { guildId, type: 'UNMUTE' } }), 0, 'a timeout expiry records no new case');

  const mute = await svc.recordInfraction({ guildId, targetUserId: U(2), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() - 1000) });
  await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: mute.id, roleId: role.id }), ctx);
  assert.equal(removed.length, 1);
  assert.match(removed[0][1], /Mute expired \(case #2\)/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: mute.id } })).active, false);
  assert.equal(await prisma.infraction.count({ where: { guildId, type: 'UNMUTE' } }), 1);
  await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: mute.id, roleId: role.id }), ctx);
  assert.equal(removed.length, 1, 'idempotent');
  assert.equal(await prisma.infraction.count({ where: { guildId, type: 'UNMUTE' } }), 1);

  // member left: the row is still closed
  const gone = await svc.recordInfraction({ guildId, targetUserId: U(3), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() - 1000) });
  await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: gone.id }), ctx);
  assert.equal((await prisma.infraction.findUnique({ where: { id: gone.id } })).active, false);

  // the mute was extended: re-deferred
  const extended = await svc.recordInfraction({ guildId, targetUserId: U(4), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() + DAY) });
  const early = await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: extended.id }), ctx);
  assert.equal(early.nextRunAt.getTime(), extended.expiresAt.getTime());

  // missing Manage Roles: hourly back-off
  const m5 = mkMember(guild, U(5), { roles: [role] });
  guild.members.cache.set(U(5), m5);
  const stuck = await svc.recordInfraction({ guildId, targetUserId: U(5), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() - 1000) });
  const real = guild.members.me.permissions;
  guild.members.me.permissions = bits('KickMembers');
  const back = await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: stuck.id }), ctx);
  assert.equal(back.payload.permRetries, 1);
  assert.equal((await prisma.infraction.findUnique({ where: { id: stuck.id } })).active, true, 'the row stays active so moderators can see it');
  guild.members.me.permissions = real;

  await assert.rejects(svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: 'x' }), ctx), PermanentJobError);

  // a transient REST failure is NOT "the member left" (rule 20): the job fails (the scheduler retries) and the row stays active
  const flaky = await svc.recordInfraction({ guildId, targetUserId: U(6), moderatorId: U(99), type: 'MUTE', expiresAt: new Date(Date.now() - 1000) });
  const realFetch = guild.members.fetch;
  guild.members.fetch = async () => { throw Object.assign(new Error('Service Unavailable'), { code: 500 }); };
  await assert.rejects(svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: flaky.id }), ctx), /Service Unavailable/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: flaky.id } })).active, true);
  assert.equal(await prisma.infraction.count({ where: { guildId, targetUserId: U(6), type: 'UNMUTE' } }), 0);
  guild.members.fetch = realFetch;
  await svc.runExpireJob(jobFor(guildId, 'mod.expire', { infractionId: flaky.id }), ctx); // 10007 now: the member really left
  assert.equal((await prisma.infraction.findUnique({ where: { id: flaky.id } })).active, false);
});

function banEntry(userId, reason = null) {
  return [userId, { reason, user: { id: userId } }];
}

test('DB: syncGuildBans imports in pages, never duplicates, and closes rows whose ban is gone', { skip: skipDb }, async () => {
  const guildId = G(9);
  const guild = mkGuild(guildId);
  const all = Array.from({ length: 1003 }, (_, i) => banEntry(String(500000000000000000n + BigInt(i)), i === 0 ? 'spam' : null));
  const calls = [];
  guild.bans.fetch = async (opts) => {
    calls.push(opts);
    const after = opts.after ? BigInt(opts.after) : -1n;
    return new Map(all.filter(([id]) => BigInt(id) > after).slice(0, opts.limit));
  };
  // one row that is still banned (must not be imported twice) and one whose ban is gone
  await svc.recordInfraction({ guildId, targetUserId: all[1][0], moderatorId: U(99), type: 'BAN' });
  const stale = await svc.recordInfraction({ guildId, targetUserId: U(77), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() + DAY) });
  await scheduleLeftover(guildId, U(77));
  await prisma.infraction.update({ where: { id: stale.id }, data: { createdAt: new Date(Date.now() - 60_000) } });
  // a row younger than the fetch is never closed, even when its ban is not in the list
  const fresh = await svc.recordInfraction({ guildId, targetUserId: U(78), moderatorId: U(99), type: 'BAN' });

  const out = await svc.syncGuildBans(guild, BOT);
  assert.deepEqual(out, { imported: 1002, closed: 1, complete: true });
  assert.equal((await prisma.infraction.findUnique({ where: { id: fresh.id } })).active, true, 'a ban recorded during the fetch survives');
  assert.equal(calls.length, 2, 'two pages');
  assert.equal(calls[0].limit, 1000);
  assert.equal(calls[1].after, all[999][0]);
  assert.equal(await prisma.infraction.count({ where: { guildId, type: 'BAN', targetUserId: all[1][0] } }), 1);
  const spam = await prisma.infraction.findFirst({ where: { guildId, targetUserId: all[0][0] } });
  assert.equal(spam.reason, '[Imported] spam');
  assert.equal(spam.moderatorId, BOT);
  assert.equal((await prisma.infraction.findUnique({ where: { id: stale.id } })).active, false);
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(77)}`), null, 'the temp-ban job of the closed row is cancelled');
  const maxCase = (await prisma.infraction.aggregate({ where: { guildId }, _max: { caseId: true }, _count: true }));
  assert.equal(maxCase._max.caseId, maxCase._count, 'case numbers stay contiguous');
  assert.deepEqual(await svc.syncGuildBans(guild, BOT), { imported: 0, closed: 0, complete: true }, 'idempotent');

  // no permission, or a failing fetch: null
  const noPerm = mkGuild(G(10));
  noPerm.members.me.permissions = bits('KickMembers');
  assert.equal(await svc.syncGuildBans(noPerm, BOT), null);
  guild.bans.fetch = async () => { throw new Error('boom'); };
  assert.equal(await svc.syncGuildBans(guild, BOT), null);
  // a guild that is not available is never reconciled
  const away = mkGuild(G(10));
  away.available = false;
  await svc.recordInfraction({ guildId: G(10), targetUserId: U(5), moderatorId: U(99), type: 'BAN' });
  away.bans.fetch = async () => new Map();
  assert.deepEqual(await svc.syncGuildBans(away, BOT), { imported: 0, closed: 0, complete: true });
  assert.equal(await prisma.infraction.count({ where: { guildId: G(10), active: true } }), 1);
});

test('DB: executeModAction: Discord first, then record; TEMPBAN schedules; failures record nothing', { skip: skipDb }, async () => {
  const guildId = G(11);
  const guild = mkGuild(guildId);
  const execOwner = mkMember(guild, OWNER);
  guild.members.cache.set(OWNER, execOwner);
  const alice = mkMember(guild, ALICE, { position: 5 });
  guild.members.cache.set(ALICE, alice);
  const mod = { id: OWNER, username: 'owner', tag: 'owner' };
  const banCalls = [];
  guild.bans.create = async (id, o) => { banCalls.push([id, o]); };
  const dms = [];
  const deleted = [];
  alice.user.send = async (p) => { dms.push(p); return { delete: async () => { deleted.push(1); } }; };

  const res = await svc.executeModAction({ guild, target: alice, moderator: mod, action: 'BAN', durationMs: 2 * DAY, reason: 'spamming', source: 'command', deleteMessageSeconds: 86400 });
  assert.equal(res.ok, true, res.error);
  assert.equal(res.action, 'TEMPBAN');
  assert.equal(res.infraction.type, 'BAN');
  assert.ok(res.infraction.expiresAt > new Date());
  assert.equal(res.dm, 'sent');
  assert.equal(dms.length, 1, 'the DM goes out before the ban');
  assert.equal(banCalls[0][1].deleteMessageSeconds, 86400);
  assert.match(banCalls[0][1].reason, /spamming \| by owner/);
  const job = await jobOf(`mod.unban:${guildId}:${ALICE}`);
  assert.equal(job.maxAttempts, 20);
  assert.equal(job.runAt.getTime(), res.infraction.expiresAt.getTime());
  assert.equal(job.payload.infractionId, res.infraction.id);
  assert.equal(wasBotBanAction(guildId, ALICE, 'ban'), true);

  // already banned -> ALREADY, nothing recorded or sent
  guild.bans.fetch = async () => ({ user: { id: ALICE } });
  const before = await prisma.infraction.count({ where: { guildId } });
  const again = await svc.executeModAction({ guild, target: alice, moderator: mod, action: 'BAN', source: 'command' });
  assert.equal(again.code, 'ALREADY');
  assert.equal(await prisma.infraction.count({ where: { guildId } }), before);
  assert.equal((await svc.executeModAction({ guild, target: alice, moderator: mod, action: 'SOFTBAN', source: 'command' })).error, MSG.SOFTBAN_BANNED);
  // a ban-list read that is refused is NOT treated as "not banned" (R025)
  guild.bans.fetch = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  const refused = await svc.executeModAction({ guild, target: alice, moderator: mod, action: 'BAN', source: 'command' });
  assert.equal(refused.code, 'NOT_PERMITTED');
  assert.match(refused.error, /Ban Members/);
  guild.bans.fetch = async () => { throw Object.assign(new Error('Unknown Ban'), { code: 10026 }); };

  // the Discord call fails: the false DM is taken back and no case is recorded (R024)
  const bob = mkMember(guild, BOB, { position: 5 });
  guild.members.cache.set(BOB, bob);
  bob.user.send = async () => ({ delete: async () => { deleted.push('bob'); } });
  guild.bans.create = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  const count = await prisma.infraction.count({ where: { guildId } });
  const failed = await svc.executeModAction({ guild, target: bob, moderator: mod, action: 'BAN', source: 'command' });
  assert.equal(failed.ok, false);
  assert.equal(failed.code, 'NOT_PERMITTED');
  assert.ok(deleted.includes('bob'), 'the DM was deleted');
  assert.equal(await prisma.infraction.count({ where: { guildId } }), count);

  // preflight refusals come back as results, with nothing recorded
  const modMember = mkMember(guild, BOB, { position: 10, perms: ['KickMembers'] });
  guild.members.cache.set(BOB, modMember);
  const adminTarget = mkMember(guild, ALICE, { position: 5, perms: ['Administrator'] });
  const immune = await svc.executeModAction({ guild, target: adminTarget, moderator: { id: BOB, username: 'bob' }, action: 'KICK', source: 'command' });
  assert.equal(immune.code, 'IMMUNE');
  const self = await svc.executeModAction({ guild, target: { id: BOB }, moderator: { id: BOB, username: 'bob' }, action: 'WARN', source: 'command' });
  assert.equal(self.error, MSG.SELF);

  // a permanent ban cancels a stale temp-ban job
  const carl = mkMember(guild, '100000000000000088', { position: 2 });
  guild.members.cache.set(carl.id, carl);
  await scheduleLeftover(guildId, carl.id);
  guild.bans.create = async () => {};
  const perm = await svc.executeModAction({ guild, target: carl, moderator: mod, action: 'BAN', source: 'command', silent: true });
  assert.equal(perm.ok, true);
  assert.equal(perm.dm, 'skipped');
  assert.equal(perm.infraction.expiresAt, null);
  assert.equal(await jobOf(`mod.unban:${guildId}:${carl.id}`), null);

  // TIMEOUT: schedules mod.expire; a newer timeout supersedes the old row and its job
  const dana = mkMember(guild, '100000000000000099', { position: 2 });
  guild.members.cache.set(dana.id, dana);
  const timeouts = [];
  dana.timeout = async (ms, why) => { timeouts.push([ms, why]); };
  const t1 = await svc.executeModAction({ guild, target: dana, moderator: mod, action: 'TIMEOUT', durationMs: HOUR, source: 'command' });
  assert.equal(t1.ok, true);
  assert.equal(timeouts[0][0], HOUR);
  assert.ok(await jobOf(`mod.expire:${t1.infraction.id}`));
  const t2 = await svc.executeModAction({ guild, target: dana, moderator: mod, action: 'TIMEOUT', durationMs: 2 * HOUR, source: 'command' });
  assert.equal((await prisma.infraction.findUnique({ where: { id: t1.infraction.id } })).active, false);
  assert.equal(await jobOf(`mod.expire:${t1.infraction.id}`), null);
  assert.ok(await jobOf(`mod.expire:${t2.infraction.id}`));
  // a timeout over 28 days without a mute role is refused
  assert.equal((await svc.executeModAction({ guild, target: dana, moderator: mod, action: 'TIMEOUT', durationMs: 40 * DAY, source: 'command' })).code, 'INVALID');
});

test('DB: executeModAction MUTE uses the mute role (one role at a time) and a long timeout falls back to it', { skip: skipDb }, async () => {
  const guildId = G(12);
  const guild = mkGuild(guildId);
  const role = mkRole('900000000000000031', 5);
  guild.roles.cache.set(role.id, role);
  const owner = mkMember(guild, OWNER);
  guild.members.cache.set(OWNER, owner);
  const eve = mkMember(guild, ALICE, { position: 2 });
  guild.members.cache.set(ALICE, eve);
  const added = [];
  eve.roles.add = async (r, why) => { added.push([r, why]); };
  const mod = { id: OWNER, username: 'owner' };

  // no mute role yet: MUTE without a duration is INVALID, with a short duration it is a timeout
  const noRole = await svc.executeModAction({ guild, target: eve, moderator: mod, action: 'MUTE', source: 'command' });
  assert.equal(noRole.code, 'INVALID');
  await updateSettings(guildId, { muteRoleId: role.id });
  const longTimeout = await svc.executeModAction({ guild, target: eve, moderator: mod, action: 'TIMEOUT', durationMs: 40 * DAY, source: 'command' });
  assert.equal(longTimeout.ok, true, longTimeout.error);
  assert.equal(longTimeout.action, 'MUTE');
  assert.match(longTimeout.warnings[0], /mute role was used instead of a timeout/);
  assert.equal(added.length, 1);
  assert.equal(added[0][0], role, 'a single role, never the array form');
  const job = await jobOf(`mod.expire:${longTimeout.infraction.id}`);
  assert.equal(job.payload.roleId, role.id);
  const forever = await svc.executeModAction({ guild, target: eve, moderator: mod, action: 'MUTE', source: 'command' });
  assert.equal(forever.ok, true);
  assert.equal(forever.infraction.expiresAt, null);
  assert.equal(await jobOf(`mod.expire:${forever.infraction.id}`), null, 'a permanent mute has no expiry job');
  assert.equal((await prisma.infraction.findUnique({ where: { id: longTimeout.infraction.id } })).active, false, 'superseded by the newer MUTE');

  // executeUnmute: removes the role, closes the rows, records an UNMUTE
  eve.roles.cache.set(role.id, role);
  const removed = [];
  eve.roles.remove = async (rid, why) => { removed.push([rid, why]); eve.roles.cache.delete(rid); };
  const un = await svc.executeUnmute({ guild, target: eve, moderator: mod, reason: 'sorry', source: 'command' });
  assert.equal(un.ok, true, un.error);
  assert.deepEqual(un.removed, ['mute role']);
  assert.equal(un.cleared, 1);
  assert.equal(removed.length, 1);
  assert.equal(un.infraction.type, 'UNMUTE');
  assert.equal((await svc.executeUnmute({ guild, target: eve, moderator: mod, source: 'command' })).code, 'ALREADY');
  // a member who left: only the records are closed
  const gone = await svc.recordInfraction({ guildId, targetUserId: U(55), moderatorId: U(99), type: 'MUTE' });
  const left = await svc.executeUnmute({ guild, target: { id: U(55) }, moderator: mod, source: 'command' });
  assert.equal(left.ok, true);
  assert.deepEqual(left.removed, []);
  assert.match(left.note, /only the records were closed/);
  assert.equal((await prisma.infraction.findUnique({ where: { id: gone.id } })).active, false);
  assert.equal((await svc.executeUnmute({ guild, target: { id: U(56) }, moderator: mod, source: 'command' })).code, 'NOT_IN_GUILD');

  // a transient member lookup failure is not "the member left": nothing is closed (rule 20)
  const stuck = await svc.recordInfraction({ guildId, targetUserId: U(57), moderatorId: U(99), type: 'MUTE' });
  const realFetch = guild.members.fetch;
  guild.members.fetch = async () => { throw Object.assign(new Error('Service Unavailable'), { code: 500 }); };
  const flaky = await svc.executeUnmute({ guild, target: { id: U(57) }, moderator: mod, source: 'command' });
  assert.equal(flaky.ok, false);
  assert.equal(flaky.code, 'DISCORD_ERROR');
  assert.equal((await prisma.infraction.findUnique({ where: { id: stuck.id } })).active, true);
  const warnRes = await svc.executeModAction({ guild, target: { id: U(57) }, moderator: mod, action: 'WARN', source: 'command' });
  assert.equal(warnRes.ok, false);
  assert.equal(warnRes.code, 'DISCORD_ERROR', 'not reported as "not in this server"');
  guild.members.fetch = realFetch;
});

test('DB: executeUnban lifts the ban, closes rows and cancels the job', { skip: skipDb }, async () => {
  const guildId = G(13);
  const guild = mkGuild(guildId);
  const mod = { id: OWNER, username: 'owner' };
  guild.members.cache.set(OWNER, mkMember(guild, OWNER));
  const row = await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() + DAY) });
  await scheduleLeftover(guildId, U(1));
  const removed = [];
  guild.bans.remove = async (id, why) => { removed.push([id, why]); };

  assert.equal((await svc.executeUnban({ guild, userId: 'x', moderator: mod })).code, 'INVALID');
  // not banned: stale rows are closed
  const stale = await svc.executeUnban({ guild, userId: U(1), moderator: mod });
  assert.equal(stale.code, 'ALREADY');
  assert.match(stale.error, /1 stale ban record\(s\) were closed/);
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(1)}`), null);
  assert.equal((await prisma.infraction.findUnique({ where: { id: row.id } })).active, false);

  // banned
  await svc.recordInfraction({ guildId, targetUserId: U(2), moderatorId: U(99), type: 'BAN' });
  guild.bans.fetch = async () => ({ user: { id: U(2), tag: 'two' } });
  const ok = await svc.executeUnban({ guild, userId: U(2), moderator: mod, reason: 'appeal', source: 'command' });
  assert.equal(ok.ok, true, ok.error);
  assert.equal(ok.cleared, 1);
  assert.equal(ok.infraction.type, 'UNBAN');
  assert.match(removed[0][1], /appeal \| by owner/);
  assert.equal(wasBotBanAction(guildId, U(2), 'unban'), true);

  // permission failures
  guild.bans.fetch = async () => { throw Object.assign(new Error('x'), { code: 50013 }); };
  assert.equal((await svc.executeUnban({ guild, userId: U(2), moderator: mod })).code, 'NOT_PERMITTED');
  guild.members.me.permissions = bits('KickMembers');
  const noPerm = await svc.executeUnban({ guild, userId: U(2), moderator: mod });
  assert.equal(noPerm.code, 'NOT_PERMITTED');
  assert.match(noPerm.error, /Ban Members/);
});

test('DB: escalation fires once per window, with severity and fallback (R152)', { skip: skipDb }, async () => {
  const guildId = G(14);
  const guild = mkGuild(guildId);
  const mod = { id: OWNER, username: 'owner' };
  guild.members.cache.set(OWNER, mkMember(guild, OWNER));
  const frank = mkMember(guild, ALICE, { position: 2 });
  guild.members.cache.set(ALICE, frank);
  const kicks = [];
  frank.kick = async (why) => { kicks.push(why); };
  await updateSettings(guildId, {
    infractionEscalation: [
      { id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null },
      { id: 2, warnCount: 4, withinSeconds: 3600, action: 'BAN', durationSeconds: null, countFrom: null },
      { id: 3, warnCount: 1, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: new Date(Date.now() + HOUR).toISOString() },
    ],
  });
  const warn = () => svc.executeModAction({ guild, target: frank, moderator: mod, action: 'WARN', reason: 'bad', source: 'command', silent: true });

  const w1 = await warn();
  assert.equal(w1.ok, true);
  assert.equal(w1.escalation, null, 'one warn is below every tier; T3 counts only warns after its countFrom (the future)');
  const w2 = await warn();
  assert.equal(w2.escalation.tierId, 1);
  assert.equal(w2.escalation.ok, true);
  assert.equal(w2.escalation.action, 'KICK');
  assert.ok(Number.isInteger(w2.escalation.caseId));
  assert.equal(kicks.length, 1);
  assert.match(kicks[0], /^\[Escalation T1\] Reached 2 active warnings/);
  const row = await prisma.infraction.findFirst({ where: { guildId, caseId: w2.escalation.caseId } });
  assert.equal(row.type, 'KICK');
  assert.equal(row.moderatorId, BOT);
  assert.match(row.reason, /^\[Escalation T1\]/);
  const w3 = await warn();
  assert.equal(w3.escalation, null, 'T1 already fired in its window: no re-fire on the next warn');
  assert.equal(kicks.length, 1);
  // a pardon (clearing the active warns) restarts a no-limit window
  await prisma.infraction.updateMany({ where: { guildId, type: 'WARN' }, data: { active: false } });
  assert.equal((await warn()).escalation, null);
  assert.equal((await warn()).escalation?.tierId, 1, 'after the pardon T1 can fire again');
  assert.equal(kicks.length, 2);

  // failure: nothing recorded, the result says so
  await prisma.infraction.updateMany({ where: { guildId, type: 'WARN' }, data: { active: false } });
  frank.kick = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  await warn();
  const failedEsc = await warn();
  assert.equal(failedEsc.escalation.ok, false);
  assert.equal(failedEsc.escalation.caseId, null);
  assert.ok(failedEsc.escalation.error);

  // module off: no escalation, but the warn still works (contract (c))
  await updateSettings(guildId, { modEnabled: false });
  await prisma.infraction.updateMany({ where: { guildId, type: 'WARN' }, data: { active: false } });
  await warn();
  const off = await warn();
  assert.equal(off.ok, true);
  assert.equal(off.escalation, null);
  await updateSettings(guildId, { modEnabled: true });

  // a legacy tier stamped by the migration ignores older warns (C8/C9)
  const g2 = G(15);
  const guild2 = mkGuild(g2);
  guild2.members.cache.set(OWNER, mkMember(guild2, OWNER));
  const target2 = mkMember(guild2, ALICE, { position: 2 });
  guild2.members.cache.set(ALICE, target2);
  const k2 = [];
  target2.kick = async (why) => { k2.push(why); };
  for (let i = 0; i < 3; i++) await svc.recordInfraction({ guildId: g2, targetUserId: ALICE, moderatorId: U(99), type: 'WARN' });
  await new Promise((r) => setTimeout(r, 20));
  await updateSettings(g2, { infractionEscalation: [{ warnCount: 2, action: 'KICK', countFrom: new Date().toISOString() }] });
  const r1 = await svc.executeModAction({ guild: guild2, target: target2, moderator: mod, action: 'WARN', source: 'command', silent: true });
  assert.equal(r1.escalation, null, '3 pre-existing warns do not count for the migrated tier');
  const r2 = await svc.executeModAction({ guild: guild2, target: target2, moderator: mod, action: 'WARN', source: 'command', silent: true });
  assert.equal(r2.escalation?.tierId, 1, '2 new warns do');
});

test('DB: external ban / unban handlers', { skip: skipDb }, async () => {
  const guildId = G(16);
  const guild = mkGuild(guildId);
  guild.members.me.permissions = bits('BanMembers'); // no ViewAuditLog: the audit lookup returns at once
  const client = guild.client;
  const settingsOn = { modEnabled: true };

  // external ban recorded (detached), unless it is ours or already recorded
  markBotBanAction(guildId, U(1), 'ban');
  await svc.recordExternalBan({ client, guild, ban: { reason: 'x' }, user: { id: U(1), tag: 'one' } });
  await svc.recordExternalBan({ client, guild, ban: { reason: 'raid', partial: false }, user: { id: U(2), tag: 'two', username: 'two' } });
  const row = await waitFor(() => prisma.infraction.findFirst({ where: { guildId, targetUserId: U(2), type: 'BAN' } }));
  assert.ok(row, 'the external ban was recorded');
  assert.equal(row.reason, '[External] raid (moderator unknown)');
  assert.equal(row.moderatorId, BOT);
  assert.equal(row.active, true);
  assert.equal(await prisma.infraction.count({ where: { guildId, targetUserId: U(1) } }), 0, 'a ban the bot made itself is not re-recorded');
  await svc.recordExternalBan({ client, guild, ban: { reason: 'again' }, user: { id: U(2), tag: 'two' } });
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(await prisma.infraction.count({ where: { guildId, targetUserId: U(2), type: 'BAN' } }), 1, 'an active BAN row means nothing new is recorded');

  // external unban: rows closed + job cancelled, UNBAN recorded when the module is on
  await scheduleLeftover(guildId, U(2));
  await svc.handleExternalUnban({ client, guild, user: { id: U(2), tag: 'two' }, settings: settingsOn });
  assert.equal((await prisma.infraction.findUnique({ where: { id: row.id } })).active, false);
  assert.equal(await jobOf(`mod.unban:${guildId}:${U(2)}`), null);
  const unban = await waitFor(() => prisma.infraction.findFirst({ where: { guildId, targetUserId: U(2), type: 'UNBAN' } }));
  assert.ok(unban);
  assert.match(unban.reason, /^\[External\]/);

  // module off: state is still resolved, nothing recorded
  const g2 = G(17);
  const guild2 = mkGuild(g2);
  guild2.members.me.permissions = bits('BanMembers');
  const r2 = await svc.recordInfraction({ guildId: g2, targetUserId: U(3), moderatorId: U(99), type: 'BAN' });
  await svc.handleExternalUnban({ client: guild2.client, guild: guild2, user: { id: U(3) }, settings: { modEnabled: false } });
  assert.equal((await prisma.infraction.findUnique({ where: { id: r2.id } })).active, false);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(await prisma.infraction.count({ where: { guildId: g2, type: 'UNBAN' } }), 0);

  // the bot's own unban (marker) is not re-recorded
  const r3 = await svc.recordInfraction({ guildId: g2, targetUserId: U(4), moderatorId: U(99), type: 'BAN' });
  markBotBanAction(g2, U(4), 'unban');
  await svc.handleExternalUnban({ client: guild2.client, guild: guild2, user: { id: U(4) }, settings: settingsOn });
  assert.equal((await prisma.infraction.findUnique({ where: { id: r3.id } })).active, false);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(await prisma.infraction.count({ where: { guildId: g2, type: 'UNBAN' } }), 0);
});

// ── /modconfig against a real database ───────────────────────────────────────

function fakeConfigInteraction(guild, { group = null, sub, opts = {}, member = null } = {}) {
  const replies = [];
  const get = (k) => (k in opts ? opts[k] : null);
  return {
    guildId: guild.id, guild, replies,
    client: guild.client,
    user: { id: OWNER, username: 'admin' },
    member: member ?? mkMember(guild, OWNER),
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      get,
      getInteger: get, getString: get, getBoolean: get, getRole: get, getChannel: get,
    },
    deferReply: async () => {},
    editReply: async (p) => { replies.push(p); },
  };
}
const embedOf = (i) => i.replies.at(-1).embeds[0].toJSON();

test('DB: /modconfig escalation add | update | remove | list', { skip: skipDb }, async () => {
  const guildId = G(18);
  const guild = mkGuild(guildId);
  const run = async (sub, opts) => { const i = fakeConfigInteraction(guild, { group: 'escalation', sub, opts }); await modconfig.execute(i); return embedOf(i); };

  let e = await run('add', { warns: 3, action: 'TIMEOUT', duration: '1h' });
  assert.match(e.title, /Escalation Tier Added/);
  assert.match(e.description, /T1: 3 warnings → Timeout for 1 hour/);
  let tiers = svc.normalizeEscalationTiers((await getSettings(guildId)).infractionEscalation);
  assert.equal(tiers[0].countFrom, null, 'new tiers count all active warnings');

  e = await run('add', { warns: 5, action: 'TEMPBAN', within: '7d', duration: '3d' });
  assert.match(e.description, /T2: 5 warnings within 1 week → Temporary ban for 3 days/);

  // updating in place keeps the id and any countFrom stamped by the migration
  await updateSettings(guildId, { infractionEscalation: tiers.map((t) => (t.id === 1 ? { ...t, countFrom: '2026-01-01T00:00:00.000Z' } : t)) });
  e = await run('add', { warns: 3, action: 'KICK' });
  assert.match(e.title, /Escalation Tier Updated/);
  tiers = svc.normalizeEscalationTiers((await getSettings(guildId)).infractionEscalation);
  const t1 = tiers.find((t) => t.warnCount === 3);
  assert.equal(t1.id, 1);
  assert.equal(t1.action, 'KICK');
  assert.equal(t1.countFrom, '2026-01-01T00:00:00.000Z');
  e = await run('list');
  assert.match(e.fields[0].value, /counts warnings since <t:/);
  assert.match(e.footer.text, /fire once per window/);

  // validation
  for (const [opts, re] of [
    [{ warns: 2, action: 'TIMEOUT' }, /Timeout tiers need a duration/],
    [{ warns: 2, action: 'TIMEOUT', duration: '30d' }, /Timeout tiers need a duration/],
    [{ warns: 2, action: 'TEMPBAN', duration: 'abc' }, /Invalid `duration`/],
    [{ warns: 2, action: 'KICK', within: '5s' }, /Invalid `within`/],
    [{ warns: 2, action: 'KICK', within: 'xyz' }, /Invalid `within`/],
    [{ warns: 2, action: 'KICK', duration: '1h' }, /don't take a duration/],
    [{ warns: 2, action: 'MUTE' }, /need a mute role/],
    [{ warns: 2, action: 'MUTE', duration: '30d' }, /need a mute role/],
  ]) {
    const out = await run('add', opts);
    assert.match(out.title, /Not Saved/, JSON.stringify(opts));
    assert.match(out.description, re, JSON.stringify(opts));
  }
  assert.equal((await run('add', { warns: 2, action: 'MUTE', duration: '2h' })).title.includes('Added'), true, 'a short mute falls back to a timeout, no mute role needed');
  await updateSettings(guildId, { muteRoleId: '900000000000000099' });
  assert.match((await run('add', { warns: 9, action: 'MUTE' })).title, /Added/);

  // cap
  for (let i = 10; i < 20; i++) await run('add', { warns: i, action: 'KICK' });
  const capped = await run('add', { warns: 50, action: 'KICK' });
  assert.match(capped.description, /at most 10 escalation tiers/);

  // remove
  e = await run('remove', { tier: 2 });
  assert.match(e.title, /Removed/);
  assert.match((await run('remove', { tier: 2 })).description, /There is no tier T2/);
  tiers = svc.normalizeEscalationTiers((await getSettings(guildId)).infractionEscalation);
  assert.equal(tiers.find((t) => t.id === 2), undefined);

  // autocomplete
  const choices = [];
  await modconfig.autocomplete({
    guildId, options: { getFocused: () => ({ name: 'tier', value: 'kick' }) }, responded: false,
    respond: async (c) => { choices.push(...c); },
    createdTimestamp: Date.now(),
  });
  assert.ok(choices.length > 0 && choices.length <= 25);
  assert.ok(choices.every((c) => /kick/i.test(c.name) && Number.isInteger(c.value)));
});

test('DB: /modconfig modroles, immune, visibility, dm, logchannel, muterole, show', { skip: skipDb }, async () => {
  const guildId = G(19);
  const guild = mkGuild(guildId);
  const role = (id, extra) => ({ ...mkRole(id, 3), ...extra });
  const r1 = role('900000000000000041');
  const r2 = role('900000000000000042', { managed: true });
  for (const r of [r1, r2]) guild.roles.cache.set(r.id, r);
  const exec = async (sub, opts, group = null) => { const i = fakeConfigInteraction(guild, { group, sub, opts }); await modconfig.execute(i); return embedOf(i); };

  let e = await exec('add', { role: r1 }, 'modroles');
  assert.match(e.title, /Role Added/);
  assert.match(e.description, /Server Settings → Integrations/);
  assert.deepEqual((await getSettings(guildId)).modRoleIds, [r1.id], 'visible at once (write-through, no cache.del)');
  assert.match((await exec('add', { role: r1 }, 'modroles')).title, /Already Listed/);
  assert.match((await exec('add', { role: r2 }, 'modroles')).description, /managed by an integration/);
  assert.match((await exec('add', { role: { id: guildId } }, 'modroles')).description, /@everyone/);
  e = await exec('list', {}, 'modroles');
  assert.match(e.description, new RegExp(r1.id));
  assert.match(e.description, /Kick, Ban or Moderate Members also count/);
  assert.match((await exec('remove', { role: r1 }, 'modroles')).title, /Role Removed/);
  assert.match((await exec('remove', { role: r1 }, 'modroles')).title, /Not Listed/);

  e = await exec('add', { role: r1 }, 'immune');
  assert.match(e.description, /cannot be warned, muted, kicked or banned/);
  // list prunes roles that the REST role list no longer contains
  await updateSettings(guildId, { immuneRoleIds: [r1.id, '900000000000000043'] });
  e = await exec('list', {}, 'immune');
  assert.match(e.description, /Removed 1 deleted role\(s\)\./);
  assert.deepEqual((await getSettings(guildId)).immuneRoleIds, [r1.id]);
  // a failed REST fetch never prunes (a cache miss is not a deletion)
  await updateSettings(guildId, { immuneRoleIds: [r1.id, '900000000000000044'] });
  const prev = guild.roles.fetch;
  guild.roles.fetch = async () => { throw new Error('rest down'); };
  await exec('list', {}, 'immune');
  assert.equal((await getSettings(guildId)).immuneRoleIds.length, 2);
  guild.roles.fetch = prev;
  // the cap
  await updateSettings(guildId, { immuneRoleIds: Array.from({ length: LIMITS.IMMUNE_ROLES }, (_, i) => `9100000000000000${String(i).padStart(2, '0')}`) });
  assert.match((await exec('add', { role: r1 }, 'immune')).description, /at most 25 roles/);

  assert.match((await exec('visibility', { mode: 'ANONYMOUS' })).description, /without the moderator/);
  assert.equal((await getSettings(guildId)).modVisibility, 'ANONYMOUS');
  await exec('dm', { enabled: false });
  assert.equal((await getSettings(guildId)).modDmTargets, false);
  assert.match((await exec('dm', { enabled: true })).description, /will be DMed/);

  // logchannel
  const ok = { id: '800000000000000051', ...fakeChannel(true) };
  ok.id = '800000000000000051';
  e = await exec('logchannel', { channel: ok });
  assert.match(e.description, /even when the Audit Logs module is off/);
  assert.equal((await getSettings(guildId)).modLogChannelId, ok.id);
  assert.equal(ok.sent.length, 1);
  const bad = fakeChannel(false);
  bad.id = '800000000000000052';
  assert.match((await exec('logchannel', { channel: bad })).description, /missing \*\*Send Messages, Embed Links\*\*/);
  assert.equal((await getSettings(guildId)).modLogChannelId, ok.id, 'an unusable channel is not saved');
  assert.match((await exec('logchannel', {})).description, /Case logging turned off/);
  assert.equal((await getSettings(guildId)).modLogChannelId, null);

  // muterole
  const muted = mkRole('900000000000000061', 3);
  guild.roles.cache.set(muted.id, muted);
  e = await exec('muterole', { role: muted });
  assert.match(e.title, /Mute Role Saved/);
  assert.equal((await getSettings(guildId)).muteRoleId, muted.id);
  const high = mkRole('900000000000000062', 500);
  assert.match((await exec('muterole', { role: high })).description, /above my highest role/);
  assert.equal((await getSettings(guildId)).muteRoleId, muted.id);
  await svc.recordInfraction({ guildId, targetUserId: U(1), moderatorId: U(99), type: 'MUTE' });
  e = await exec('muterole', { clear: true });
  assert.match(e.description, /1 role mute\(s\) are still active/);
  assert.equal((await getSettings(guildId)).muteRoleId, null);
  assert.match((await exec('muterole', {})).description, /No mute role is set/);

  // setup:true on a role: overwrites on categories and children, failures counted
  const edits = [];
  const mkCh = (id, type, can = true) => ({
    id, type, guild, isThread: () => false, isTextBased: () => true,
    permissionsFor: () => (can ? bits('ViewChannel', 'ManageRoles') : bits('ViewChannel')),
    permissionOverwrites: { edit: async (r, deny, o) => { edits.push([id, r.id, deny, o]); } },
  });
  guild.channels.cache.set('c1', mkCh('c1', 4)); // category
  guild.channels.cache.set('c2', mkCh('c2', 0)); // text
  guild.channels.cache.set('c3', mkCh('c3', 2)); // voice
  guild.channels.cache.set('c4', mkCh('c4', 0, false)); // no access
  guild.channels.cache.set('c5', { ...mkCh('c5', 11), isThread: () => true }); // thread: skipped
  e = await exec('muterole', { role: muted, setup: true });
  assert.match(e.description, /\*\*Channel overrides:\*\* 3 applied, 1 failed \(missing access\)/);
  assert.equal(edits.length, 3);
  assert.equal(edits[0][2].SendMessages, false);
  assert.equal(edits[0][2].Speak, false);

  // show
  await updateSettings(guildId, { infractionEscalation: [{ id: 1, warnCount: 2, withinSeconds: null, action: 'KICK', durationSeconds: null, countFrom: null }], modLogChannelId: ok.id, muteRoleId: '900000000000000077' });
  guild.channels.cache.set(ok.id, ok);
  e = await exec('show');
  assert.match(e.title, /Moderation Settings/);
  const field = (n) => e.fields.find((f) => f.name === n).value;
  assert.match(field('Module'), /On/);
  assert.equal(field('Case log'), `<#${ok.id}>`);
  assert.equal(field('Mute role'), '⚠️ deleted');
  assert.equal(field('Visibility'), 'ANONYMOUS');
  assert.match(field('Escalation'), /T1: 2 warnings → Kick/);
  assert.equal(e.description, undefined);
  await svc.recordInfraction({ guildId, targetUserId: U(2), moderatorId: U(99), type: 'BAN', expiresAt: new Date(Date.now() - 45 * DAY) });
  e = await exec('show');
  assert.match(e.description, /1 temporary ban\(s\) expired more than 30 days ago and were not lifted automatically\. Use \/unban to lift them\./);
});

test('DB: deliver helpers survive a deleted interaction', { skip: skipDb }, async () => {
  const gone = { deleteReply: async () => { throw Object.assign(new Error('x'), { code: 10062 }); }, followUp: async () => { throw Object.assign(new Error('x'), { code: 10062 }); }, editReply: async () => {} };
  await assert.doesNotReject(svc.deliverModReply(gone, await getSettings(G(19)), { title: 't', lines: ['l'] }));
});
