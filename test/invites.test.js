// pkg-invites tests (pkg-invites.md §8): the pure snapshot/attribution functions (T1-T12), the command payload (T13),
// the plugin declaration (T14), the removed legacy exports (T15), the store/runtime helpers against fake guilds, a static scan,
// and (TEST_DATABASE_URL, a migrated database) the SQL, the attribution end to end, and the /invites command flows.
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/invites.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails closed/open as documented, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';
const GUILD_PREFIX = '95501000000000';                      // every guild id of this file starts with this

const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/inviteTracker.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const cmd = await import('../src/commands/utility/invites.js');
const { plugin } = await import('../src/plugins/invites.js');

const {
  FAKE_THRESHOLD_MS, TOMBSTONE_TTL_MS, MAX_PENDING_JOINS,
  isSnapshot, isTrustedSnapshot, inviteToState, emptySnapshot, buildSnapshot, applyInviteCreate, applyInviteDelete,
  resolveJoin, classifyJoin, computeStats, refreshSnapshot, recordInviteCreate, recordInviteDelete, getCodeState,
  deleteDiscordInvite, trackJoin, trackLeave, getInviteStats, getInviteLeaderboard, getJoinRecord, getPersonalInvite,
  savePersonalInvite, botCanTrack,
} = svc;

// Silence the Redis reconnect and logger noise.
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};
after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // Rows cascade from GuildSettings; deleted once, at the very end (ensureGuildSettings remembers ensured guilds).
    await prisma.guildSettings.deleteMany({ where: { guildId: { startsWith: GUILD_PREFIX } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ──────────────────────────────────────────────────────────────────

const NOW = 10_000_000;
const S = (codes, extra = {}) => ({ v: 1, takenAt: 1000, codes, tombstones: {}, vanity: null, ...extra });
const c = (uses, maxUses = 0, inviterId = null, expiresAt = null) => ({ uses, maxUses, inviterId, expiresAt });
const o = (code, uses, maxUses = 0, inviterId = null, expiresAt = null) => ({ code, uses, maxUses, inviterId, expiresAt });
const obs = (list, vanity) => ({ invites: list, vanity });
const clone = (x) => structuredClone(x);

// ── T1-T12: pure functions ───────────────────────────────────────────────────

test('T1 R139: a missing-baseline bug cannot misattribute: bob 2 -> 3 is the match', () => {
  const before = S({ alice: c(5, 0, 'A'), bob: c(2, 0, 'B') });
  const r = resolveJoin(before, obs([o('alice', 5), o('bob', 3)]), { now: NOW });
  assert.equal(r.match.code, 'bob');
  assert.equal(r.match.inviterId, 'B');
  assert.equal(r.reason, 'matched');
  assert.equal(r.next.codes.bob.uses, 3);
});

test('T1b: the legacy { code: uses } value is not a snapshot', () => {
  assert.equal(isSnapshot({ alice: 5, bob: 2 }), false);
  assert.equal(isSnapshot(null), false);
  assert.equal(isSnapshot([]), false);
  assert.equal(isSnapshot(S({}, { vanity: undefined })), false);
  assert.equal(isSnapshot(S({}, { vanity: { code: 'x', uses: 1.5 } })), false);
  assert.equal(isSnapshot(S({}, { vanity: { code: 'x', uses: 3 } })), true);
  assert.equal(isSnapshot(S({})), true);
});

test('T2 R142: alice99 50 / bob123 3 -> 4 is bob123', () => {
  const r = resolveJoin(S({ alice99: c(50), bob123: c(3) }), obs([o('alice99', 50), o('bob123', 4)]), { now: NOW });
  assert.equal(r.match.code, 'bob123');
});

test('T2b: a snapshot from before this process became ready is untrusted', () => {
  assert.equal(isTrustedSnapshot(S({}, { takenAt: 1000 }), 2000), false);
  assert.equal(isTrustedSnapshot(S({}, { takenAt: 1000 }), 1000), true);
  assert.equal(isTrustedSnapshot({ alice: 1 }, 0), false);
  assert.equal(isTrustedSnapshot(S({}), Number.POSITIVE_INFINITY), false);
});

test('T3 R143: a consumed single-use invite (vanished at uses = max - 1) is the match and is marked exhausted', () => {
  const before = S({ single: c(0, 1, 'C'), perm: c(5) });
  const r = resolveJoin(before, obs([o('perm', 5)]), { now: NOW });
  assert.equal(r.match.code, 'single');
  assert.equal(r.match.kind, 'vanished');
  assert.equal(r.match.inviterId, 'C');
  assert.equal(r.next.tombstones.single.uses, 1);
});

test('T3b/T3c: the inviteDelete tombstone matches for 2 minutes only', () => {
  const before = S({ single: c(0, 1, 'C'), perm: c(5) });
  const b2 = applyInviteDelete(before, 'single', { now: NOW });
  assert.equal(resolveJoin(b2, obs([o('perm', 5)]), { now: NOW + 1000 }).match.code, 'single');
  assert.equal(resolveJoin(b2, obs([o('perm', 5)]), { now: NOW + TOMBSTONE_TTL_MS }).match.code, 'single');
  assert.equal(resolveJoin(b2, obs([o('perm', 5)]), { now: NOW + TOMBSTONE_TTL_MS + 1 }).reason, 'none');
});

test('T3d: an exhausted tombstone can never match a second join', () => {
  const before = S({ single: c(0, 1, 'C'), perm: c(5) });
  const first = resolveJoin(before, obs([o('perm', 5)]), { now: NOW });
  assert.equal(first.reason, 'matched');
  const again = resolveJoin(first.next, obs([o('perm', 5)]), { now: NOW + 1000 });   // explicit now: the tombstone is still alive
  assert.ok(again.next.tombstones.single, 'the tombstone is still stored');
  assert.equal(again.reason, 'none');
});

test('T4 R144: Carol\'s single-use invite is credited to Carol', () => {
  const r = resolveJoin(S({ alice: c(5), once: c(0, 1, 'Carol') }), obs([o('alice', 5)]), { now: NOW });
  assert.equal(r.match.code, 'once');
  assert.equal(r.match.inviterId, 'Carol');
});

test('T4b/T4c: a moderator-deleted (not exhausted) or expired vanished invite is not a candidate', () => {
  assert.equal(resolveJoin(S({ gone: c(3, 10, 'X') }), obs([]), { now: NOW }).reason, 'none');
  assert.equal(resolveJoin(S({ gone: c(0, 1, 'X', NOW - 1) }), obs([]), { now: NOW }).reason, 'none');
  assert.equal(resolveJoin(S({ gone: c(0, 1, 'X', NOW + 1) }), obs([]), { now: NOW }).reason, 'matched');
  assert.equal(resolveJoin(S({ gone: c(0, 0, 'X') }), obs([]), { now: NOW }).reason, 'none');          // unlimited invite
});

test('T5 R276: two increments in one observation are ambiguous, never "the first one"', () => {
  const r = resolveJoin(S({ alice: c(5), bob: c(2) }), obs([o('alice', 6), o('bob', 3)]), { now: NOW });
  assert.equal(r.match, null);
  assert.equal(r.reason, 'ambiguous');
  assert.equal(r.candidates.length, 2);
  assert.equal(r.next.codes.alice.uses, 6);
  assert.equal(r.next.codes.bob.uses, 3);
});

test('T5b: an ambiguous join still marks the vanished candidate exhausted', () => {
  const r = resolveJoin(S({ alice: c(5), v: c(0, 1, 'V') }), obs([o('alice', 6)]), { now: NOW });
  assert.equal(r.match, null);
  assert.equal(r.reason, 'ambiguous');
  assert.equal(r.next.tombstones.v.uses, 1);
});

test('T6 R140/R145: the vanity baseline is kept and a vanity join is matched against it', () => {
  const before = S({ alice: c(5) }, { vanity: { code: 'cool', uses: 40 } });
  const r = resolveJoin(before, obs([o('alice', 5)], { code: 'cool', uses: 41 }), { now: NOW });
  assert.equal(r.match.kind, 'vanity');
  assert.equal(r.match.code, 'cool');
  assert.equal(r.match.inviterId, null);
  assert.equal(r.next.vanity.uses, 41);

  // T6b: the next join is alice's and the baseline stays 41
  const r2 = resolveJoin(r.next, obs([o('alice', 6)], { code: 'cool', uses: 41 }), { now: NOW });
  assert.equal(r2.match.code, 'alice');
  assert.equal(r2.next.vanity.uses, 41);

  // T6c: a Server Discovery join moves nothing: unknown, never the vanity URL
  const r3 = resolveJoin(r2.next, obs([o('alice', 6)], { code: 'cool', uses: 41 }), { now: NOW });
  assert.equal(r3.reason, 'none');
});

test('T6d: incremental edits keep the vanity baseline; an unchanged vanity is not a candidate', () => {
  const before = S({ alice: c(5) }, { vanity: { code: 'cool', uses: 900 } });
  const created = applyInviteCreate(before, { code: 'new', uses: 0, maxUses: 0, inviterId: 'N', expiresAt: null }, { now: NOW });
  assert.equal(created.vanity.uses, 900);
  assert.equal(resolveJoin(created, obs([o('alice', 5), o('new', 0)], { code: 'cool', uses: 900 }), { now: NOW }).reason, 'none');
});

test('T6e: a vanity +1 together with an invite +1 is ambiguous', () => {
  const before = S({ alice: c(5) }, { vanity: { code: 'cool', uses: 40 } });
  const r = resolveJoin(before, obs([o('alice', 6)], { code: 'cool', uses: 41 }), { now: NOW });
  assert.equal(r.reason, 'ambiguous');
});

test('T6f: an unknown vanity baseline never matches, and is learned for the next join', () => {
  const r = resolveJoin(S({ alice: c(5) }), obs([o('alice', 5)], { code: 'cool', uses: 41 }), { now: NOW });
  assert.equal(r.reason, 'none');
  assert.equal(r.next.vanity.uses, 41);
});

test('T6g: a failed vanity fetch (undefined) and a changed vanity code store no baseline', () => {
  assert.equal(buildSnapshot(obs([o('a', 1)], undefined), { now: NOW }).vanity, null);
  assert.equal(buildSnapshot(obs([o('a', 1)], null), { now: NOW }).vanity, null);
  const before = S({ a: c(1) }, { vanity: { code: 'old', uses: 5 } });
  assert.equal(resolveJoin(before, obs([o('a', 1)], { code: 'new', uses: 9 }), { now: NOW }).reason, 'none');
});

test('T7: an invite created after the snapshot (applyInviteCreate) matches on its first use', () => {
  const created = applyInviteCreate(S({ alice: c(5) }), { code: 'new', uses: 0, maxUses: 0, inviterId: 'N', expiresAt: null }, { now: NOW });
  const r = resolveJoin(created, obs([o('alice', 5), o('new', 1, 0, 'N')]), { now: NOW });
  assert.equal(r.match.code, 'new');
  assert.equal(r.match.inviterId, 'N');
});

test('T7b: applyInviteCreate keeps an existing entry (its uses may be higher) and never changes takenAt', () => {
  const before = S({ alice: c(9, 0, 'A') });
  const after = applyInviteCreate(before, { code: 'alice', uses: 0, maxUses: 0, inviterId: 'Z', expiresAt: null }, { now: NOW });
  assert.equal(after.codes.alice.uses, 9);
  assert.equal(after.codes.alice.inviterId, 'A');
  assert.equal(after.takenAt, 1000);
});

test('T7c: a code absent from the snapshot with uses 1 matches (baseline 0)', () => {
  const r = resolveJoin(S({ alice: c(5) }), obs([o('alice', 5), o('fresh', 1, 0, 'F')]), { now: NOW });
  assert.equal(r.match.code, 'fresh');
});

test('T8: buildSnapshot tombstones vanished codes only from a (trusted) previous snapshot', () => {
  const previous = S({ a: c(1), b: c(0, 1) });
  const built = buildSnapshot(obs([o('a', 1)]), { previous, now: NOW });
  assert.equal(built.tombstones.b.deletedAt, NOW);
  assert.deepEqual(buildSnapshot(obs([o('a', 1)]), { previous: null, now: NOW }).tombstones, {});
  // old tombstones are pruned, fresh ones carried over
  const prev2 = S({}, { tombstones: { old: { ...c(0, 1), deletedAt: NOW - TOMBSTONE_TTL_MS - 1 }, fresh: { ...c(0, 1), deletedAt: NOW - 10 } } });
  const built2 = buildSnapshot(obs([]), { previous: prev2, now: NOW });
  assert.deepEqual(Object.keys(built2.tombstones), ['fresh']);
});

test('T9: resolveJoin and the apply* functions never mutate their inputs', () => {
  const before = S({ single: c(0, 1, 'C'), perm: c(5), alice: c(1) }, { vanity: { code: 'cool', uses: 3 } });
  const observation = obs([o('perm', 5), o('alice', 2)], { code: 'cool', uses: 3 });
  const b = clone(before);
  const ob = clone(observation);
  resolveJoin(before, observation, { now: NOW });
  applyInviteCreate(before, { code: 'n', uses: 0, maxUses: 0, inviterId: null, expiresAt: null }, { now: NOW });
  applyInviteDelete(before, 'perm', { now: NOW });
  buildSnapshot(observation, { previous: before, now: NOW });
  assert.deepEqual(before, b);
  assert.deepEqual(observation, ob);
});

test('T10: a JSON round trip keeps the snapshot valid and resolveJoin unchanged', () => {
  const before = S({ alice: c(5, 0, 'A'), single: c(0, 1, 'C') }, { vanity: { code: 'cool', uses: 3 } });
  const observation = obs([o('alice', 5, 0, 'A')], { code: 'cool', uses: 3 });
  const trip = JSON.parse(JSON.stringify(before));
  assert.equal(isSnapshot(trip), true);
  const a = resolveJoin(before, observation, { now: NOW });
  const b = resolveJoin(trip, observation, { now: NOW });
  assert.deepEqual(b, a);
  assert.equal(emptySnapshot(7).takenAt, 7);
  assert.equal(isSnapshot(emptySnapshot(7)), true);
});

test('T11: classifyJoin follows the 24 h rule and the self-invite rule', () => {
  const row = (extra) => ({ userId: '1', inviterId: '2', joinedAt: 0, leftAt: null, ...extra });
  assert.equal(classifyJoin(row()), 'active');
  assert.equal(classifyJoin(row({ leftAt: FAKE_THRESHOLD_MS - 1 })), 'fake');
  assert.equal(classifyJoin(row({ leftAt: FAKE_THRESHOLD_MS })), 'left');
  assert.equal(classifyJoin(row({ inviterId: '1' })), 'fake');
  assert.equal(classifyJoin(row({ inviterId: '1', leftAt: FAKE_THRESHOLD_MS * 3 })), 'fake');
  assert.equal(classifyJoin(row({ joinedAt: new Date(0), leftAt: new Date(FAKE_THRESHOLD_MS) })), 'left');
});

test('T12: computeStats over the six reference rows', () => {
  const now = 1_000_000_000_000;
  const H = 3_600_000;
  const rows = [
    { userId: 'a', inviterId: 'I', joinedAt: now - H, leftAt: null },
    { userId: 'b', inviterId: 'I', joinedAt: now - 48 * H, leftAt: null },
    { userId: 'c', inviterId: 'I', joinedAt: now - 100 * H, leftAt: now - 70 * H },
    { userId: 'd', inviterId: 'I', joinedAt: now - 100 * H, leftAt: now - 98 * H },
    { userId: 'I', inviterId: 'I', joinedAt: now - 10 * H, leftAt: null },
    { userId: 'e', inviterId: null, joinedAt: now - H, leftAt: null },
  ];
  assert.deepEqual(computeStats(rows, { now }), { total: 5, active: 2, left: 1, fake: 2, valid: 3, recent: 1 });
});

test('inviteToState reads the invite defensively', () => {
  assert.deepEqual(inviteToState({ code: 'x' }), { code: 'x', uses: 0, maxUses: 0, inviterId: null, expiresAt: null });
  assert.deepEqual(
    inviteToState({ code: 'x', uses: 2, maxUses: 5, inviter: { id: 'U' }, expiresTimestamp: 99 }),
    { code: 'x', uses: 2, maxUses: 5, inviterId: 'U', expiresAt: 99 },
  );
});

test('a code that is a prototype property name cannot poison the lookup', () => {
  const r = resolveJoin(S({}), obs([o('constructor', 0), o('toString', 1, 0, 'T')]), { now: NOW });
  assert.equal(r.match.code, 'toString');
});

// ── T13: the command payload ─────────────────────────────────────────────────

const charCount = (node) => {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const ch of node.choices ?? []) n += ch.name.length + String(ch.value).length;
  for (const op of node.options ?? []) n += charCount(op);
  return n;
};

test('T13: /invites payload, exports and limits', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'invites');
  assert.deepEqual(json.options.map((x) => x.name), ['show', 'inviter', 'leaderboard', 'link']);
  assert.deepEqual(json.contexts, [0]);
  assert.ok(charCount(json) <= 500, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'invites');
  assert.equal(cmd.gated, true);
  assert.equal(cmd.permissionLevel, 0);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(json.default_member_permissions ?? null, null);
  const sub = Object.fromEntries(json.options.map((x) => [x.name, x]));
  assert.equal(sub.inviter.options[0].name, 'member');
  assert.equal(sub.inviter.options[0].required, true);
  assert.notEqual(sub.show.options[0].required, true);
  assert.equal(sub.leaderboard.options[0].min_value, 1);
  assert.equal(sub.leaderboard.options[0].max_value, 20);
  assert.deepEqual(sub.link.options[0].channel_types, [0, 5, 2, 13]);
  for (const s of json.options) {
    assert.ok(s.options.length <= 1);
    for (const op of s.options) assert.ok(!op.choices && !op.autocomplete);
  }
  assert.equal(typeof cmd.execute, 'function');
});

// ── T14: the plugin ──────────────────────────────────────────────────────────

test('T14: the plugin loads cleanly with its five hooks', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('invites')), []);
  const p = plugins.find((x) => x.id === 'invites');
  assert.ok(p, 'invites plugin present');
  assert.equal(p.module, 'invites');
  assert.deepEqual(p.hooks.map((h) => `${h.event}:${h.id}`).sort(),
    ['guildMemberAdd:invites', 'guildMemberRemove:invites', 'guildReady:invites', 'inviteCreate:invites', 'inviteDelete:invites']);
  const hook = (ev) => p.hooks.find((h) => h.event === ev);
  assert.equal(hook('guildMemberAdd').always, true);
  assert.equal(hook('guildMemberRemove').always, true);
  assert.notEqual(hook('inviteCreate').always, true);
  assert.deepEqual(p.components, []);
  assert.deepEqual(p.jobs, []);
  assert.deepEqual(p.provides, {});
});

test('plugin: bots are not tracked on join, and the leave hook filters them', async () => {
  const join = plugin.hooks.find((h) => h.event === 'guildMemberAdd');
  const ctx = { state: Object.create(null), member: { id: 'B', user: { bot: true }, guild: { id: '955010000000009999' } }, getSettings: async () => null };
  await join.run(ctx);
  assert.equal(ctx.state.invite, null);
  const leave = plugin.hooks.find((h) => h.event === 'guildMemberRemove');
  assert.equal(leave.filter({ user: { bot: true } }), false);
  assert.equal(leave.filter({ user: { bot: false } }), true);
  assert.equal(leave.filter({ member: { user: { bot: true } } }), false);
  assert.equal(leave.filter({}), true);
});

// ── T15: the legacy exports are gone ─────────────────────────────────────────

test('T15: the legacy cacheGuildInvites / getUsedInvite exports were removed (attribution runs in the hooks)', async () => {
  assert.equal('cacheGuildInvites' in svc, false);
  assert.equal('getUsedInvite' in svc, false);
  assert.equal(typeof svc.refreshSnapshot, 'function');
  // Nothing under src/ still calls them.
  const walk = (dir) => readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)]));
  const users = walk(resolve(ROOT, 'src')).filter((file) => /\b(?:cacheGuildInvites|getUsedInvite)\b/.test(readFileSync(file, 'utf8')));
  assert.deepEqual(users, []);
});

// ── Runtime helpers against fake guilds (the memo is the store: Redis is down) ──

function fakeGuild(id, { invites = [], vanityURLCode = null, vanityUses = 0, perms = true, readyTimestamp = 0, botId = 'BOT' } = {}) {
  const g = {
    id,
    available: true,
    vanityURLCode,
    live: new Map(invites.map((i) => [i.code, { maxUses: 0, uses: 0, inviterId: null, expiresTimestamp: null, ...i }])),
    vanityUses,
    failFetch: null,
    deleted: [],
    fetchCalls: 0,
    client: { readyTimestamp, user: { id: botId }, fetchInvite: async () => { throw Object.assign(new Error('Unknown Invite'), { code: 10006 }); } },
    members: { me: { permissions: { has: () => perms } } },
    invites: {
      fetch: async () => {
        g.fetchCalls += 1;
        if (g.failFetch) throw g.failFetch;
        return new Map(g.live);
      },
      delete: async (code, reason) => { g.deleted.push({ code, reason }); },
    },
    fetchVanityData: async () => ({ code: g.vanityURLCode, uses: g.vanityUses }),
  };
  return g;
}

test('refreshSnapshot warms a snapshot; incremental edits touch no REST; no snapshot means no edit', async () => {
  const g = fakeGuild('955010000000000101', { invites: [{ code: 'A', uses: 2, inviterId: 'U1' }] });
  // no snapshot yet: create/delete edits do nothing (never a partial snapshot)
  await recordInviteCreate(g, { code: 'B', uses: 0, maxUses: 0, inviterId: 'U2' });
  assert.equal(await getCodeState(g, 'B', { allowFetch: false }), 'unknown');

  assert.deepEqual(await refreshSnapshot(g), { ok: true });
  assert.equal(g.fetchCalls, 1);
  assert.equal(await getCodeState(g, 'A', { allowFetch: false }), 'live');

  await recordInviteCreate(g, { code: 'B', uses: 0, maxUses: 0, inviterId: 'U2' });
  assert.equal(await getCodeState(g, 'B', { allowFetch: false }), 'live');
  await recordInviteDelete(g, { code: 'B' });
  assert.equal(await getCodeState(g, 'B', { allowFetch: false }), 'gone');
  assert.equal(g.fetchCalls, 1, 'the incremental edits made no REST call');
});

test('refreshSnapshot reports why it did nothing and keeps the stored snapshot', async () => {
  const g = fakeGuild('955010000000000102', { invites: [{ code: 'A' }] });
  g.available = false;
  assert.deepEqual(await refreshSnapshot(g), { ok: false, reason: 'unavailable' });
  g.available = true;
  const noPerm = fakeGuild('955010000000000103', { perms: false });
  assert.equal(botCanTrack(noPerm), false);
  assert.deepEqual(await refreshSnapshot(noPerm), { ok: false, reason: 'no-permission' });
  assert.equal(noPerm.fetchCalls, 0);

  assert.deepEqual(await refreshSnapshot(g), { ok: true });
  g.failFetch = Object.assign(new Error('Missing Access'), { code: 50001 });
  assert.deepEqual(await refreshSnapshot(g), { ok: false, reason: 'fetch-failed' });
  assert.equal(await getCodeState(g, 'A', { allowFetch: false }), 'live', 'the old snapshot survives a failed fetch');
  assert.equal(botCanTrack({ members: { me: null } }), true, 'an uncached me is optimistic');
});

test('a snapshot from before this process became ready is replaced, not diffed or inherited', async () => {
  const g = fakeGuild('955010000000000104', { invites: [{ code: 'A', maxUses: 1, uses: 0 }], readyTimestamp: 0 });
  await refreshSnapshot(g);
  // A restart: the shard becomes ready later than the stored snapshot. A vanished code must NOT become a tombstone.
  g.client.readyTimestamp = Date.now() + 60_000;
  g.live.delete('A');
  assert.deepEqual(await refreshSnapshot(g), { ok: true });
  assert.equal(await getCodeState(g, 'A', { allowFetch: false }), 'unknown');
});

test('getCodeState: snapshot rules, the since rule and the public fetch', async () => {
  const g = fakeGuild('955010000000000105', { invites: [{ code: 'A' }] });
  assert.equal(await getCodeState(g, 'X', { allowFetch: false }), 'unknown');           // no snapshot
  await refreshSnapshot(g);
  assert.equal(await getCodeState(g, 'X', { allowFetch: false, since: Date.now() - 60_000 }), 'gone');   // observed after it was created
  assert.equal(await getCodeState(g, 'X', { allowFetch: false, since: Date.now() + 60_000 }), 'unknown');
  assert.equal(await getCodeState(g, 'X', { allowFetch: false }), 'unknown');

  g.client.fetchInvite = async () => ({ guild: { id: g.id } });
  assert.equal(await getCodeState(g, 'X', { since: Date.now() + 60_000 }), 'live');
  g.client.fetchInvite = async () => ({ guild: { id: 'other' } });
  assert.equal(await getCodeState(g, 'X', { since: Date.now() + 60_000 }), 'gone');
  g.client.fetchInvite = async () => { throw Object.assign(new Error('nope'), { code: 10006 }); };
  assert.equal(await getCodeState(g, 'X', { since: Date.now() + 60_000 }), 'gone');
  g.client.fetchInvite = async () => { throw new Error('network'); };
  assert.equal(await getCodeState(g, 'X', { since: Date.now() + 60_000 }), 'unknown');
});

test('deleteDiscordInvite swallows every error', async () => {
  const g = { id: 'g', invites: { delete: async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); } } };
  await deleteDiscordInvite(g, 'X', 'why');
  const ok = fakeGuild('955010000000000106');
  await deleteDiscordInvite(ok, 'X', 'why');
  assert.deepEqual(ok.deleted, [{ code: 'X', reason: 'why' }]);
});

// ── Static scan ──────────────────────────────────────────────────────────────

test('static: no timers, no console, no reserved alias, every COUNT cast to int', () => {
  const read = (f) => readFileSync(resolve(ROOT, f), 'utf8');
  const service = read('src/services/inviteTracker.js');
  const plug = read('src/plugins/invites.js');
  const command = read('src/commands/utility/invites.js');
  for (const [name, src] of [['service', service], ['plugin', plug]]) {
    assert.ok(!/setTimeout|setInterval/.test(src), `${name}: no raw timers`);
  }
  for (const [name, src] of [['service', service], ['plugin', plug], ['command', command]]) {
    assert.ok(!/\bconsole\./.test(src), `${name}: no console`);
  }
  assert.ok(!/AS "left"/.test(service));
  // A raw COUNT is a BigInt: every selected aggregate is cast to int (the VALID helper is cast where it is selected).
  for (const alias of ['total', 'active', 'leftCount', 'fake', 'recent', 'valid']) {
    assert.ok(new RegExp(`::int AS "${alias}"`).test(service), `${alias} is cast to int`);
  }
  assert.equal([...service.matchAll(/COUNT\(/g)].length, 6);
  for (const m of service.matchAll(/Prisma\.sql\s*`([\s\S]*?)`/g)) {
    assert.ok(!/(?<![\w.])now\(\)|CURRENT_TIMESTAMP/i.test(m[1]), 'no bare now() in SQL');
  }
  assert.ok(/SQL_NOW/.test(service));
  assert.ok(/ensureGuildSettings\(gid\)/.test(service) && /ensureGuildSettings\(guildId\)/.test(service));
});

// ── DB cases ─────────────────────────────────────────────────────────────────

const H = 3_600_000;

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  assert.ok(Array.isArray(svc.RAW_SQL) && svc.RAW_SQL.length === 2);
  for (const { name, build } of svc.RAW_SQL) {
    const q = build();
    assert.ok(typeof q.sql === 'string' && Array.isArray(q.values), `${name} builds a Prisma.Sql`);
    await prisma.$queryRaw(q);
  }
});

async function seedRows(guildId, rows) {
  await prisma.guildSettings.upsert({ where: { guildId }, update: {}, create: { guildId } });
  await prisma.guildMemberCache.createMany({ data: rows.map((r) => ({ guildId, ...r })) });
}

test('DB: getInviteStats matches computeStats row for row, and returns plain numbers', { skip: skipDb }, async () => {
  const guildId = '955010000000000201';
  const now = Date.now();
  const rows = [
    { userId: 'a', inviterId: 'I1', joinedAt: new Date(now - H), leftAt: null },
    { userId: 'b', inviterId: 'I1', joinedAt: new Date(now - 48 * H), leftAt: null },
    { userId: 'c', inviterId: 'I1', joinedAt: new Date(now - 100 * H), leftAt: new Date(now - 70 * H) },
    { userId: 'd', inviterId: 'I1', joinedAt: new Date(now - 100 * H), leftAt: new Date(now - 98 * H) },
    { userId: 'I1', inviterId: 'I1', joinedAt: new Date(now - 10 * H), leftAt: null },
    { userId: 'e', inviterId: null, joinedAt: new Date(now - H), leftAt: null },
    { userId: 'f', inviterId: 'I2', joinedAt: new Date(now - 5 * H), leftAt: null },
  ];
  await seedRows(guildId, rows);
  const sql = await getInviteStats(guildId, 'I1');
  assert.deepEqual(sql, { total: 5, active: 2, left: 1, fake: 2, valid: 3, recent: 1 });
  assert.deepEqual(sql, computeStats(rows.filter((r) => r.inviterId === 'I1'), { now }));
  for (const v of Object.values(sql)) assert.equal(typeof v, 'number');
  assert.deepEqual(await getInviteStats(guildId, 'nobody'), { total: 0, active: 0, left: 0, fake: 0, valid: 0, recent: 0 });
  assert.deepEqual(await getInviteStats('955010000000000299', 'I1'), { total: 0, active: 0, left: 0, fake: 0, valid: 0, recent: 0 });
});

test('DB: the leaderboard ranks by valid invites, excludes ids and zero-valid inviters, and honours the limit', { skip: skipDb }, async () => {
  const guildId = '955010000000000202';
  const now = Date.now();
  const mk = (userId, inviterId, extra = {}) => ({ userId, inviterId, joinedAt: new Date(now - 50 * H), leftAt: null, ...extra });
  await seedRows(guildId, [
    mk('a1', 'I1'), mk('a2', 'I1'), mk('a3', 'I1', { leftAt: new Date(now - 10 * H) }),                 // valid 3 (left after 40 h)
    mk('b1', 'I5'), mk('b2', 'I5'),                                                                     // valid 2
    mk('c1', 'I2'), mk('c2', 'I2'),                                                                     // valid 2 (ties I5; I2 < I5)
    mk('d1', 'I3', { joinedAt: new Date(now - 5 * H), leftAt: new Date(now - 4 * H) }),                 // fake only
    mk('e1', 'BOT'), mk('e2', 'BOT'), mk('e3', 'BOT'), mk('e4', 'BOT'),                                 // valid 4, excluded below
    { userId: 'z', inviterId: null, joinedAt: new Date(now), leftAt: null },
  ]);
  const rows = await getInviteLeaderboard(guildId, { limit: 200, excludeIds: ['BOT'] });
  assert.deepEqual(rows.map((r) => [r.inviterId, r.valid]), [['I1', 3], ['I2', 2], ['I5', 2]]);
  assert.deepEqual(rows[0], { inviterId: 'I1', total: 3, active: 2, left: 1, fake: 0, valid: 3, recent: 0 });
  assert.equal((await getInviteLeaderboard(guildId, { excludeIds: [] }))[0].inviterId, 'BOT');
  assert.equal((await getInviteLeaderboard(guildId)).length, 4);
  assert.equal((await getInviteLeaderboard(guildId, { limit: 2, excludeIds: ['BOT'] })).length, 2);
  assert.deepEqual(await getInviteLeaderboard('955010000000000298'), []);
});

test('DB: savePersonalInvite upserts per member and resets createdAt', { skip: skipDb }, async () => {
  const guildId = '955010000000000203';
  const first = await savePersonalInvite({ guildId, userId: 'U1', code: 'CODE1', channelId: 'c1' });
  assert.equal((await getPersonalInvite(guildId, 'U1')).code, 'CODE1');
  await new Promise((r) => setTimeout(r, 15));
  const second = await savePersonalInvite({ guildId, userId: 'U1', code: 'CODE2', channelId: 'c2' });
  assert.equal(second.id, first.id);
  assert.equal(second.code, 'CODE2');
  assert.equal(second.channelId, 'c2');
  assert.ok(second.createdAt.getTime() > first.createdAt.getTime(), 'createdAt was reset');
  assert.equal(await getPersonalInvite(guildId, 'nobody'), null);
});

function fakeMember(guild, id, { bot = false, joinedAt = new Date() } = {}) {
  return { id, guild, joinedAt, user: { id, bot } };
}

test('DB: attribution end to end (snapshot, match, ambiguity, vanity, personal link, consumed invite, bot, rejoin)', { skip: skipDb }, async () => {
  const gid = '955010000000000301';
  const g = fakeGuild(gid, {
    invites: [{ code: 'A', uses: 0, inviterId: 'U1' }, { code: 'B', uses: 0, inviterId: 'U2' }],
    vanityURLCode: 'cool', vanityUses: 10,
  });
  const row = (id) => getJoinRecord(gid, id);

  // 1. no snapshot yet: unknown, but the row is written and a trusted snapshot is taken
  assert.equal(await trackJoin(fakeMember(g, 'm1')), null);
  assert.deepEqual([(await row('m1')).inviterId, (await row('m1')).inviteCode, (await row('m1')).leftAt], [null, null, null]);

  // 2. a single increment is matched
  g.live.get('A').uses = 1;
  assert.deepEqual(await trackJoin(fakeMember(g, 'm2')), { code: 'A', inviterId: 'U1', vanity: false });
  assert.deepEqual([(await row('m2')).inviterId, (await row('m2')).inviteCode], ['U1', 'A']);

  // 3. two increments: ambiguous, recorded as unknown
  g.live.get('A').uses = 2;
  g.live.get('B').uses = 1;
  assert.equal(await trackJoin(fakeMember(g, 'm3')), null);
  assert.equal((await row('m3')).inviterId, null);

  // 4. vanity join against the baseline learned above
  g.vanityUses = 11;
  assert.deepEqual(await trackJoin(fakeMember(g, 'm4')), { code: 'cool', inviterId: null, vanity: true });
  assert.deepEqual([(await row('m4')).inviterId, (await row('m4')).inviteCode], [null, 'cool']);

  // 5. a discovery join (nothing moved) is unknown
  assert.equal(await trackJoin(fakeMember(g, 'm5')), null);

  // 6. a personal link (created by the bot) is credited to its owner; a bot-created code without a row is not credited
  await savePersonalInvite({ guildId: gid, userId: 'U3', code: 'PERS', channelId: 'c1' });
  g.live.set('PERS', { code: 'PERS', uses: 0, maxUses: 0, inviterId: 'BOT', expiresTimestamp: null });
  await recordInviteCreate(g, g.live.get('PERS'));
  g.live.get('PERS').uses = 1;
  assert.deepEqual(await trackJoin(fakeMember(g, 'm6')), { code: 'PERS', inviterId: 'U3', vanity: false });
  g.live.set('BOTLINK', { code: 'BOTLINK', uses: 0, maxUses: 0, inviterId: 'BOT', expiresTimestamp: null });
  await recordInviteCreate(g, g.live.get('BOTLINK'));
  g.live.get('BOTLINK').uses = 1;
  assert.deepEqual(await trackJoin(fakeMember(g, 'm7')), { code: 'BOTLINK', inviterId: null, vanity: false });

  // 7. a consumed single-use invite vanishes from the list (R143/R144)
  const once = { code: 'ONCE', uses: 0, maxUses: 1, inviterId: 'U4', expiresTimestamp: null };
  g.live.set('ONCE', once);
  await recordInviteCreate(g, once);
  g.live.delete('ONCE');
  assert.deepEqual(await trackJoin(fakeMember(g, 'm8')), { code: 'ONCE', inviterId: 'U4', vanity: false });
  assert.equal(await trackJoin(fakeMember(g, 'm9')), null, 'the exhausted code cannot match a second join');

  // 8. D3: a rejoin overwrites the attribution and clears leftAt
  await trackLeave(g, 'm2');
  assert.notEqual((await row('m2')).leftAt, null);
  assert.equal(await trackJoin(fakeMember(g, 'm2')), null);
  const again = await row('m2');
  assert.deepEqual([again.inviterId, again.inviteCode, again.leftAt], [null, null, null]);

  // 9. a leave never creates a row
  await trackLeave(g, 'never-joined');
  assert.equal(await row('never-joined'), null);
});

test('DB: a join followed at once by a leave is processed in that order (the leaver is fake)', { skip: skipDb }, async () => {
  const gid = '955010000000000302';
  const g = fakeGuild(gid, { invites: [{ code: 'A', uses: 0, inviterId: 'U1' }] });
  await refreshSnapshot(g);
  g.live.get('A').uses = 1;
  const joined = trackJoin(fakeMember(g, 'quick'));             // no await in between
  const left = trackLeave(g, 'quick');
  assert.deepEqual(await joined, { code: 'A', inviterId: 'U1', vanity: false });
  await left;
  const r = await getJoinRecord(gid, 'quick');
  assert.notEqual(r.leftAt, null);
  assert.equal(classifyJoin(r), 'fake');
  assert.deepEqual(await getInviteStats(gid, 'U1'), { total: 1, active: 0, left: 0, fake: 1, valid: 0, recent: 0 });
});

test('DB: concurrent joins are serialised, and a raid beyond the cap never queues more REST diffs', { skip: skipDb }, async () => {
  const gid = '955010000000000303';
  const g = fakeGuild(gid, { invites: [{ code: 'A', uses: 0, inviterId: 'U1' }] });
  await refreshSnapshot(g);
  const before = g.fetchCalls;
  const total = MAX_PENDING_JOINS + 4;
  const results = await Promise.all(Array.from({ length: total }, (_, i) => trackJoin(fakeMember(g, `raid${i}`))));
  assert.ok(results.every((r) => r === null));
  assert.equal(g.fetchCalls - before, MAX_PENDING_JOINS, 'only the first MAX_PENDING_JOINS joins fetched');
  assert.equal(await prisma.guildMemberCache.count({ where: { guildId: gid } }), total, 'every join still has a row');
  // the burst dropped the snapshot: the next normal join re-snapshots and is unknown, the one after is attributed
  assert.equal(await trackJoin(fakeMember(g, 'after1')), null);
  g.live.get('A').uses = 1;
  assert.deepEqual(await trackJoin(fakeMember(g, 'after2')), { code: 'A', inviterId: 'U1', vanity: false });
});

test('DB: a disabled module records unknown rows and drops the snapshot; re-enabling re-baselines', { skip: skipDb }, async () => {
  const gid = '955010000000000304';
  const g = fakeGuild(gid, { invites: [{ code: 'A', uses: 0, inviterId: 'U1' }] });
  await refreshSnapshot(g);
  g.live.get('A').uses = 1;
  assert.equal(await trackJoin(fakeMember(g, 'off1'), { isEnabled: async () => false }), null);
  const r = await getJoinRecord(gid, 'off1');
  assert.equal(r.inviterId, null);
  assert.equal(await getCodeState(g, 'A', { allowFetch: false }), 'unknown', 'the snapshot was dropped');
  assert.equal(await trackJoin(fakeMember(g, 'on1')), null);          // first enabled join: no snapshot -> unknown, re-baselined
  g.live.get('A').uses = 2;
  assert.equal((await trackJoin(fakeMember(g, 'on2'))).code, 'A');
});

test('DB: a missing Manage Server permission records the row without attribution', { skip: skipDb }, async () => {
  const gid = '955010000000000305';
  const g = fakeGuild(gid, { invites: [{ code: 'A', uses: 0, inviterId: 'U1' }], perms: false });
  assert.equal(await trackJoin(fakeMember(g, 'np')), null);
  assert.equal(g.fetchCalls, 0);
  assert.equal((await getJoinRecord(gid, 'np')).inviterId, null);
});

test('DB: through runHooks the join hook enqueues first, so join then leave keeps gateway order', { skip: skipDb }, async () => {
  const { registerHook, clearHooks, runHooks } = await import('../src/handlers/hookRegistry.js');
  const gid = '955010000000000306';
  const g = fakeGuild(gid, { invites: [{ code: 'A', uses: 0, inviterId: 'U1' }] });
  clearHooks();
  try {
    for (const h of plugin.hooks) registerHook(h, { pluginId: 'invites', pluginModule: 'invites' });
    const ready = await runHooks('guildReady', { client: g.client, guild: g, reason: 'startup' });
    assert.equal(ready.stopped, false);
    assert.equal(await getCodeState(g, 'A', { allowFetch: false }), 'live', 'guildReady warmed the snapshot');

    await runHooks('inviteCreate', { client: g.client, guild: g, invite: { code: 'N', uses: 0, maxUses: 0, inviterId: 'U2' } });
    assert.equal(await getCodeState(g, 'N', { allowFetch: false }), 'live');
    await runHooks('inviteDelete', { client: g.client, guild: g, invite: { code: 'N' } });
    assert.equal(await getCodeState(g, 'N', { allowFetch: false }), 'gone');

    g.live.get('A').uses = 1;
    const member = fakeMember(g, 'hooked');
    // Neither dispatch is awaited before the next starts: the second must still queue behind the first.
    const join = runHooks('guildMemberAdd', { client: g.client, guild: g, member, user: member.user });
    const leave = runHooks('guildMemberRemove', { client: g.client, guild: g, member, user: member.user });
    const [joinCtx] = await Promise.all([join, leave]);
    assert.deepEqual(joinCtx.state.invite, { code: 'A', inviterId: 'U1', vanity: false });
    const r = await getJoinRecord(gid, 'hooked');
    assert.equal(r.inviterId, 'U1');
    assert.notEqual(r.leftAt, null, 'leftAt was set after the join row existed');

    // a bot never gets a row and ctx.state.invite stays null
    const bot = fakeMember(g, 'botuser', { bot: true });
    const botCtx = await runHooks('guildMemberAdd', { client: g.client, guild: g, member: bot, user: bot.user });
    assert.equal(botCtx.state.invite, null);
    await runHooks('guildMemberRemove', { client: g.client, guild: g, member: bot, user: bot.user });
    assert.equal(await getJoinRecord(gid, 'botuser'), null);
  } finally {
    clearHooks();
  }
});

// ── DB: the /invites command through fake interactions ────────────────────────

function fakeInteraction(guild, { sub, user = { id: 'viewer', tag: 'viewer#0', username: 'viewer', globalName: null }, member, channel, options = {} } = {}) {
  const i = {
    guild,
    user,
    member: member ?? { displayName: 'Viewer', permissions: { has: () => true } },
    channel,
    client: { user: { id: 'BOT' } },
    createdTimestamp: Date.now(),
    deferred: false,
    replied: false,
    deferOpts: null,
    replies: [],
    options: {
      getSubcommand: () => sub,
      getUser: (n) => options.user ?? null,
      getMember: () => options.member ?? null,
      getInteger: () => options.page ?? null,
      getChannel: () => options.channel ?? null,
    },
    isRepliable: () => true,
    deferReply: async (o) => { i.deferred = true; i.deferOpts = o ?? null; },
    editReply: async (p) => { i.replies.push(p); return { id: 'msg' }; },
  };
  return i;
}

const fieldMap = (embed) => Object.fromEntries((embed.data.fields ?? []).map((f) => [f.name, f.value]));

test('DB: /invites show, inviter and leaderboard render the stored data', { skip: skipDb }, async () => {
  const gid = '955010000000000401';
  const g = fakeGuild(gid, { vanityURLCode: 'cool' });
  g.name = 'Test Guild';
  const now = Date.now();
  await seedRows(gid, [
    { userId: 'a', inviterId: 'I1', inviteCode: 'CODE', joinedAt: new Date(now - 2 * H), leftAt: null },
    { userId: 'b', inviterId: 'I1', inviteCode: 'CODE', joinedAt: new Date(now - 100 * H), leftAt: new Date(now - 98 * H) },
    { userId: 'v', inviterId: null, inviteCode: 'cool', joinedAt: new Date(now - 100 * H), leftAt: null },
    { userId: 'o', inviterId: null, inviteCode: 'ORPH', joinedAt: new Date(now - 100 * H), leftAt: null },
    { userId: 'n', inviterId: null, inviteCode: null, joinedAt: new Date(now - 100 * H), leftAt: null },
    { userId: 'gone', inviterId: 'I1', inviteCode: 'CODE', joinedAt: new Date(now - 100 * H), leftAt: new Date(now - 60 * H) },
  ]);
  await savePersonalInvite({ guildId: gid, userId: 'I1', code: 'PERSONAL', channelId: 'chan1' });

  // show: stats of I1 (a active/recent, gone left after 40 h = valid, b fake)
  const show = fakeInteraction(g, { sub: 'show', options: { user: { id: 'I1', username: 'ivy', globalName: 'Ivy' } } });
  await cmd.execute(show);
  assert.equal(show.deferOpts, null, 'show is public');
  const e = show.replies[0].embeds[0];
  assert.match(e.data.title, /Invites — Ivy/);
  assert.match(e.data.description, /<@I1> has \*\*2\*\* valid invites\./);
  assert.match(e.data.description, /1 of them joined less than 24 hours ago/);
  const f = fieldMap(e);
  assert.equal(f['✅ Valid'], '2');
  assert.equal(f['🏠 Still here'], '1');
  assert.equal(f['🚪 Left (after 24 h)'], '1');
  assert.equal(f['❌ Fake'], '1');
  assert.equal(f['📥 Total joins'], '3');
  assert.match(f['🙋 Invited by'], /^Unknown/);
  assert.match(f['🔗 Personal link'], /https:\/\/discord\.gg\/PERSONAL \(opens <#chan1>\)/);

  // show on a member the bot knows the inviter of
  const showA = fakeInteraction(g, { sub: 'show', options: { user: { id: 'a', username: 'al', globalName: null } } });
  await cmd.execute(showA);
  assert.match(fieldMap(showA.replies[0].embeds[0])['🙋 Invited by'], /^<@I1> · `discord\.gg\/CODE` · joined <t:\d+:R>$/);

  // inviter
  const texts = {};
  for (const id of ['a', 'v', 'o', 'n', 'gone', 'missing']) {
    const it = fakeInteraction(g, { sub: 'inviter', options: { user: { id, username: id } } });
    await cmd.execute(it);
    texts[id] = it.replies[0].embeds[0].data.description;
  }
  assert.match(texts.a, /^<@a> was invited by <@I1> with `discord\.gg\/CODE` <t:\d+:R>\.$/);
  assert.match(texts.v, /^<@v> joined through the vanity URL `discord\.gg\/cool` <t:\d+:R>\.$/);
  assert.match(texts.o, /^<@o> joined with `discord\.gg\/ORPH` <t:\d+:R>, but I don't know who created that invite\.$/);
  assert.match(texts.n, /^I don't know who invited <@n>\./);
  assert.match(texts.gone, /\nThey left <t:\d+:R>\.$/);
  assert.match(texts.missing, /^I don't know who invited <@missing>\./);

  // leaderboard
  const lb = fakeInteraction(g, { sub: 'leaderboard', user: { id: 'I1', tag: 'i#0', username: 'i' } });
  await cmd.execute(lb);
  const lbEmbed = lb.replies[0].embeds[0];
  assert.match(lbEmbed.data.title, /Test Guild — Invite Leaderboard/);
  assert.match(lbEmbed.data.description, /^🥇 <@I1> — \*\*2\*\* invites \(1 left · 1 fake\)$/);
  assert.match(lbEmbed.data.footer.text, /Valid = still here or stayed 24 h · Your rank: #1/);

  // an empty guild
  const empty = fakeInteraction(fakeGuild('955010000000000402'), { sub: 'leaderboard' });
  await cmd.execute(empty);
  assert.match(empty.replies[0].embeds[0].data.description, /No invites have been tracked/);
});

test('DB: /invites link creates, reuses and replaces a personal link and maps every failure', { skip: skipDb }, async () => {
  const gid = '955010000000000403';
  const g = fakeGuild(gid);
  g.systemChannel = null;
  let created = 0;
  const mkChannel = (id, { type = 0, createInvite, member = true, bot = true } = {}) => ({
    id, type, guild: g, isThread: () => false,
    permissionsFor: (who) => ({ has: () => (who === g.members.me ? bot : member) }),
    createInvite: createInvite ?? (async () => { created += 1; return { code: `NEW${created}` }; }),
  });
  const chan = mkChannel('c1');
  const user = { id: 'LU', tag: 'lu#0', username: 'lu' };
  const run = async (extra = {}) => {
    const it = fakeInteraction(g, { sub: 'link', user, channel: chan, ...extra });
    await cmd.execute(it);
    return it;
  };

  // create
  const first = await run();
  assert.deepEqual(first.deferOpts, { ephemeral: true });
  assert.equal(first.replies[0].content, 'https://discord.gg/NEW1');
  assert.deepEqual(first.replies[0].allowedMentions, { parse: [] });
  assert.equal((await getPersonalInvite(gid, 'LU')).code, 'NEW1');

  // reuse: the invite is alive (public fetch), nothing is created
  g.client.fetchInvite = async () => ({ guild: { id: gid } });
  const again = await run();
  assert.equal(again.replies[0].content, 'https://discord.gg/NEW1');
  assert.equal(again.replies[0].embeds[0].data.footer.text, 'This is your existing link.');
  assert.equal(created, 1);

  // an unknown state (network error) still reuses: a duplicate is never created
  g.client.fetchInvite = async () => { throw new Error('network'); };
  assert.equal((await run()).replies[0].content, 'https://discord.gg/NEW1');
  assert.equal(created, 1);

  // the link died: a new one replaces it (the old code is already gone, so no "deleted" footer)
  g.client.fetchInvite = async () => { throw Object.assign(new Error('Unknown Invite'), { code: 10006 }); };
  const replaced = await run();
  assert.equal(replaced.replies[0].content, 'https://discord.gg/NEW2');
  assert.equal(created, 2);
  assert.deepEqual(g.deleted.map((d) => d.code), ['NEW1']);
  assert.equal((await getPersonalInvite(gid, 'LU')).code, 'NEW2');

  // a different channel replaces a live link and says so
  g.client.fetchInvite = async () => ({ guild: { id: gid } });
  const other = mkChannel('c2');
  const moved = await run({ options: { channel: other } });
  assert.equal(moved.replies[0].content, 'https://discord.gg/NEW3');
  assert.match(moved.replies[0].embeds[0].data.footer.text, /Your previous link discord\.gg\/NEW2 was deleted\./);
  assert.match(moved.replies[0].embeds[0].data.description, /Opens <#c2>/);

  // failures
  const title = (it) => it.replies[0].embeds[0].data.title;
  const failing = (code) => mkChannel('c3', { createInvite: async () => { throw Object.assign(new Error('x'), { code }); } });
  g.client.fetchInvite = async () => { throw Object.assign(new Error('Unknown Invite'), { code: 10006 }); };
  assert.match(title(await run({ options: { channel: failing(30016) } })), /Invite Limit Reached/);
  assert.match(title(await run({ options: { channel: failing(50013) } })), /Missing Permissions/);
  assert.match(title(await run({ options: { channel: failing(10003) } })), /Channel Not Found/);
  assert.match(title(await run({ options: { channel: failing(99999) } })), /Something Went Wrong/);
  assert.match(title(await run({ options: { channel: mkChannel('c4', { type: 4 }) } })), /Unsupported Channel/);
  assert.match(title(await run({ options: { channel: mkChannel('c5', { bot: false }) } })), /Missing Permissions/);
  assert.match(title(await run({ options: { channel: mkChannel('c6', { member: false }) } })), /Not Allowed/);
  assert.equal((await getPersonalInvite(gid, 'LU')).code, 'NEW3', 'failures leave the stored link alone');

  // no usable channel at all
  const nowhere = fakeInteraction(g, { sub: 'link', user: { id: 'LU2', tag: 'x#0', username: 'x' }, channel: mkChannel('c7', { member: false }) });
  await cmd.execute(nowhere);
  assert.match(title(nowhere), /No Channel Available/);
});
