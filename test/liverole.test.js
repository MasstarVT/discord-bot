// pkg-liverole tests (pkg-liverole.md §8). Offline by default (no Discord, Postgres or Redis): the pure helpers, the formatters, reconcileMember
// with fake guilds, the flap guard, the /liverole payload, the plugin declaration and static source checks.
// With TEST_DATABASE_URL (a migrated database) the command flows also run end to end through the real settingsService:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/liverole.test.js
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable, so settings load as null.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const { ActivityType } = await import('discord.js');
const lr = await import('../src/plugins/liverole.js');
const cmd = await import('../src/commands/admin/liverole.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');

const {
  LIVEROLE, REASONS, isStreaming, streamingChanged, shouldHandlePresence, streamLabel, meetsRequirement, planMember, planSync,
  classifyRoleError, formatSyncResult, liveRoleProblems, reconcileMember, countLosingHolders, loadMembers, syncGuild, plugin,
} = lr;

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// Guild ids created by the DB cases (below); deleted before Prisma disconnects.
const DB_GUILDS = [];

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  if (process.env.TEST_DATABASE_URL && DB_GUILDS.length) {
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: DB_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

const S = { activities: [{ type: 1 }] };
const P = { activities: [{ type: 0 }] };

// ── 1. isStreaming ────────────────────────────────────────────────────────────

test('ActivityType values the spec relies on', () => {
  assert.equal(ActivityType.Streaming, 1);
  assert.equal(ActivityType.Playing, 0);
});

test('isStreaming', () => {
  for (const v of [null, undefined, {}, { activities: [] }, { activities: [{ type: 0 }] }, { activities: 'x' }, { activities: [null] }]) {
    assert.equal(isStreaming(v), false, JSON.stringify(v));
  }
  assert.equal(isStreaming({ activities: [{ type: 1 }] }), true);
  assert.equal(isStreaming({ activities: [{ type: 0 }, { type: 1, name: 'Twitch' }] }), true);
});

test('streamingChanged treats a null old presence as not streaming', () => {
  assert.equal(streamingChanged(null, S), true);
  assert.equal(streamingChanged(null, P), false);
  assert.equal(streamingChanged(S, S), false);
});

// ── 2. shouldHandlePresence ───────────────────────────────────────────────────

test('shouldHandlePresence truth table', () => {
  const on = { liveRoleEnabled: true, liveRoleId: '1' };
  const rows = [
    [null, S, null, true],
    [null, P, null, false],
    [S, S, null, false],
    [S, P, null, true],
    [P, S, { liveRoleEnabled: false, liveRoleId: '1' }, false],
    [P, S, { liveRoleEnabled: true, liveRoleId: null }, false],
    [P, S, on, true],
    [S, { status: 'offline', activities: [] }, null, true],
    [S, P, on, true],
    [S, S, on, false],
  ];
  for (const [o, n, peeked, expected] of rows) {
    assert.equal(shouldHandlePresence(o, n, peeked), expected, JSON.stringify([o, n, peeked]));
  }
});

// ── 3. meetsRequirement ───────────────────────────────────────────────────────

test('meetsRequirement', () => {
  assert.equal(meetsRequirement({ requiredRoleId: null, memberRoleIds: new Set(), guildId: 'G' }), true);
  assert.equal(meetsRequirement({ requiredRoleId: 'G', memberRoleIds: new Set(), guildId: 'G' }), true);
  assert.equal(meetsRequirement({ requiredRoleId: 'r', memberRoleIds: new Set(['r']), guildId: 'G' }), true);
  assert.equal(meetsRequirement({ requiredRoleId: 'r', memberRoleIds: new Set(['x']), guildId: 'G' }), false);
  assert.equal(meetsRequirement({ requiredRoleId: 'r', requiredRoleExists: false, memberRoleIds: new Set(['r']), guildId: 'G' }), false);
});

// ── 4. planMember ─────────────────────────────────────────────────────────────

test('planMember over every combination', () => {
  for (const streaming of [false, true]) {
    for (const qualifies of [false, true]) {
      for (const holds of [false, true]) {
        const want = streaming && qualifies;
        const expected = want && !holds ? 'add' : (!want && holds ? 'remove' : null);
        assert.equal(planMember({ mode: 'sync', streaming, qualifies, holds }), expected, `${streaming}/${qualifies}/${holds}`);
        assert.equal(planMember({ mode: 'clear', streaming, qualifies, holds }), holds ? 'remove' : null);
      }
    }
  }
  assert.equal(planMember({ streaming: true, qualifies: true, holds: false }), 'add'); // mode defaults to sync
});

// ── 5. planSync ───────────────────────────────────────────────────────────────

test('planSync', () => {
  const base = { holderIds: ['a', 'b'], streamerIds: ['b', 'c', 'd'], unknownIds: ['e', 'a'] };
  assert.deepEqual(planSync({ ...base, qualifies: (id) => id !== 'd' }), { add: ['c'], remove: ['a'], check: ['e'] });
  assert.ok(planSync({ ...base, qualifies: (id) => id !== 'b' }).remove.includes('b'), 'a streaming holder who no longer qualifies loses it');
  assert.deepEqual(planSync({ mode: 'clear', holderIds: ['a', 'a', 'b'] }), { add: [], remove: ['a', 'b'], check: [] });
  // de-duplicated, input order kept
  assert.deepEqual(planSync({ streamerIds: ['x', 'y', 'x'], qualifies: () => true }), { add: ['x', 'y'], remove: [], check: [] });
});

// ── 6. classifyRoleError ──────────────────────────────────────────────────────

test('classifyRoleError', () => {
  assert.equal(classifyRoleError({ code: 10007 }), 'gone');
  assert.equal(classifyRoleError({ code: 10011 }), 'role-missing');
  assert.equal(classifyRoleError({ code: 50013 }), 'perms');
  assert.equal(classifyRoleError({ code: 50001 }), 'perms');
  assert.equal(classifyRoleError({ code: 500 }), 'error');
  assert.equal(classifyRoleError(new Error('x')), 'error');
  assert.equal(classifyRoleError(null), 'error');
});

// ── 7. formatSyncResult ───────────────────────────────────────────────────────

test('formatSyncResult', () => {
  const ok = { ok: true, mode: 'sync', added: 0, removed: 0, failed: 0, deferred: 0, addsBlocked: null, complete: true };
  assert.match(formatSyncResult(null), /Still syncing/);
  assert.equal(formatSyncResult(ok), 'Everyone is already in sync.');
  assert.match(formatSyncResult({ ...ok, added: 2, removed: 1 }), /Synced: gave it to \*\*2\*\*, removed it from \*\*1\*\*\./);
  assert.match(formatSyncResult({ ...ok, deferred: 3 }), /`\/liverole enable`/);
  assert.match(formatSyncResult({ ...ok, mode: 'clear', deferred: 3 }), /`\/liverole disable`/);
  assert.equal(formatSyncResult({ ...ok, mode: 'clear' }), 'Nobody had the live role.');
  assert.match(formatSyncResult({ ...ok, mode: 'clear', removed: 4 }), /Removed the live role from \*\*4\*\* member\(s\)\./);
  assert.match(formatSyncResult({ ...ok, failed: 2 }), /\*\*2\*\* change\(s\) failed/);
  assert.match(formatSyncResult({ ...ok, addsBlocked: 'dangerous' }), /Not giving the role to anyone: dangerous/);
  assert.match(formatSyncResult({ ...ok, complete: false }), /Only cached members were checked/);
  const reason = 'That role is above my highest role.';
  assert.ok(formatSyncResult({ ok: false, code: 'cannot-manage', reason }).includes(reason));
  assert.match(formatSyncResult({ ok: false, code: 'settings' }), /settings unavailable/);
  assert.match(formatSyncResult({ ok: false, code: 'off' }), /live role is off/);
  assert.match(formatSyncResult({ ok: false, code: 'no-role' }), /no live role is set/);
  assert.match(formatSyncResult({ ok: false, code: 'role-missing' }), /was deleted/);
  assert.match(formatSyncResult({ ok: false, code: 'error' }), /Sync failed/);
});

// ── 8. liveRoleProblems ───────────────────────────────────────────────────────

test('liveRoleProblems', () => {
  const good = {
    enabled: true, liveRoleId: '1', roleExists: true, manage: { ok: true }, dangerousNames: [], requiredRoleId: null, requiredRoleExists: true, holderCount: 2,
  };
  assert.deepEqual(liveRoleProblems(good), []);
  assert.deepEqual(liveRoleProblems({ ...good, liveRoleId: null, roleExists: false }), ['⚠️ The module is on but no live role is set. Use `/liverole set`.']);
  assert.deepEqual(liveRoleProblems({ ...good, roleExists: false }), ['⚠️ The live role was deleted. Pick a new one with `/liverole set`.']);
  assert.deepEqual(
    liveRoleProblems({ ...good, manage: { ok: false, reason: 'That role is above my highest role.' } }),
    ["⚠️ That role is above my highest role. I can't give or remove the live role until this is fixed."],
  );
  assert.deepEqual(
    liveRoleProblems({ ...good, dangerousNames: ['Manage Roles', 'Kick Members'] }),
    ["⚠️ The live role has dangerous permissions (Manage Roles, Kick Members). I won't give it to anyone until they are removed."],
  );
  assert.deepEqual(
    liveRoleProblems({ ...good, requiredRoleId: 'r', requiredRoleExists: false }),
    ['⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.'],
  );
  assert.deepEqual(
    liveRoleProblems({ ...good, enabled: false, holderCount: 3 }),
    ['ℹ️ **3** member(s) still have the live role. `/liverole disable` removes it from them.'],
  );
  assert.deepEqual(liveRoleProblems({ ...good, enabled: false, holderCount: 0 }), []);
});

// ── 9. streamLabel ────────────────────────────────────────────────────────────

test('streamLabel', () => {
  assert.equal(streamLabel({ activities: [{ type: 1, name: 'Twitch' }] }), 'Twitch');
  assert.ok(streamLabel({ activities: [{ type: 1, name: 'x'.repeat(100) }] }).length <= 32);
  assert.equal(streamLabel({ activities: [{ type: 1 }] }), 'Streaming');
  assert.equal(streamLabel({ activities: [{ type: 1, name: '*bold*' }] }), '\\*bold\\*');
  assert.equal(streamLabel(P), null);
  assert.equal(streamLabel(null), null);
});

// ── 10. reconcileMember with fakes ────────────────────────────────────────────

/** A fake guild with one role 'L', one member 'U', and the bot above the role. */
function world({ roleHeld = false, streaming = false, requiredRoleId = null, extraRoles = [] } = {}) {
  const calls = [];
  const role = { id: 'L', position: 1, managed: false, permissions: { has: () => false }, members: new Map() };
  const me = { permissions: { has: () => true }, roles: { highest: { position: 10 } } };
  const roleIds = new Set(roleHeld ? ['L', ...extraRoles] : extraRoles);
  const member = {
    id: 'U',
    partial: false,
    user: { bot: false },
    roles: {
      cache: roleIds,
      add: async (id, reason) => { calls.push(['add', id, reason]); roleIds.add(id); },
      remove: async (id, reason) => { calls.push(['remove', id, reason]); roleIds.delete(id); },
    },
  };
  const guild = {
    id: 'G',
    ownerId: 'O',
    roles: { cache: new Map([['L', role], ...(requiredRoleId ? [[requiredRoleId, { id: requiredRoleId }]] : [])]) },
    members: { me, cache: new Map([['U', member]]), fetch: async () => null },
    presences: { cache: new Map() },
  };
  if (streaming) guild.presences.cache.set('U', { activities: [{ type: 1, name: 'Twitch' }] });
  const settings = { liveRoleEnabled: true, liveRoleId: 'L', liveRoleRequiredRoleId: requiredRoleId };
  return { calls, role, me, member, guild, settings };
}

const rec = (w, extra = {}) => reconcileMember(w.guild, 'U', { mode: 'sync', settings: w.settings, source: 'presence', ...extra });

test('reconcileMember: streaming and role not held adds it, one string id, idempotent', async () => {
  const w = world({ streaming: true });
  assert.equal(await rec(w), 'added');
  assert.deepEqual(w.calls, [['add', 'L', REASONS.presenceAdd]]);
  assert.equal(typeof w.calls[0][1], 'string');
  assert.equal(await rec(w), 'none');
  assert.equal(w.calls.length, 1);
});

test('reconcileMember: not streaming and role held removes it', async () => {
  const w = world({ roleHeld: true });
  assert.equal(await rec(w), 'removed');
  assert.deepEqual(w.calls, [['remove', 'L', REASONS.presenceRemove]]);
  assert.equal(await rec(w), 'none');
  assert.equal(w.calls.length, 1);
});

test('reconcileMember: sync-sourced calls use the sync reasons, clear mode uses the clear reason', async () => {
  const a = world({ streaming: true });
  assert.equal(await rec(a, { source: 'sync' }), 'added');
  assert.equal(a.calls[0][2], REASONS.syncAdd);
  const b = world({ roleHeld: true });
  assert.equal(await rec(b, { source: 'sync' }), 'removed');
  assert.equal(b.calls[0][2], REASONS.syncRemove);
  const c = world({ roleHeld: true, streaming: true });
  assert.equal(await rec(c, { source: 'sync', mode: 'clear' }), 'removed');
  assert.equal(c.calls[0][2], REASONS.clear);
});

test('reconcileMember: a dangerous role is never added but can be removed', async () => {
  const a = world({ streaming: true });
  a.role.permissions.has = () => true;
  assert.equal(await rec(a), 'blocked');
  assert.equal(a.calls.length, 0);
  const b = world({ roleHeld: true });
  b.role.permissions.has = () => true;
  assert.equal(await rec(b), 'removed');
  assert.equal(b.calls.length, 1);
});

test('reconcileMember: a bot role at or below the live role blocks both directions with no REST call', async () => {
  const a = world({ streaming: true });
  a.me.roles.highest.position = 0;
  assert.equal(await rec(a), 'blocked');
  const b = world({ roleHeld: true });
  b.me.roles.highest.position = 0;
  assert.equal(await rec(b), 'blocked');
  assert.equal(a.calls.length + b.calls.length, 0);
});

test('reconcileMember: a Discord error is classified and never thrown', async () => {
  const w = world({ streaming: true });
  w.member.roles.add = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  assert.equal(await rec(w), 'failed');
  const g = world({ streaming: true });
  g.member.roles.add = async () => { throw Object.assign(new Error('Unknown Member'), { code: 10007 }); };
  assert.equal(await rec(g), 'gone');
  const r = world({ streaming: true });
  r.member.roles.add = async () => { throw Object.assign(new Error('Unknown Role'), { code: 10011 }); };
  assert.equal(await rec(r), 'role-missing');
  const e = world({ streaming: true });
  e.member.roles.add = async () => { throw new Error('boom'); };
  assert.equal(await rec(e), 'failed');
});

test('reconcileMember: bots, missing members and missing roles', async () => {
  const bot = world({ streaming: true });
  bot.member.user.bot = true;
  assert.equal(await rec(bot), 'none');
  assert.equal(bot.calls.length, 0);

  const gone = world({ streaming: true });
  gone.guild.members.cache.delete('U');
  assert.equal(await rec(gone), 'gone');

  const noRole = world({ streaming: true });
  noRole.guild.roles.cache.delete('L');
  assert.equal(await rec(noRole), 'role-missing');
  assert.equal(noRole.calls.length, 0);
  assert.equal(noRole.settings.liveRoleId, 'L', 'a cache miss never clears the setting');
});

test('reconcileMember: a partial or uncached member is fetched before any role logic', async () => {
  const w = world({ streaming: true });
  const real = w.member;
  w.guild.members.cache.set('U', { ...real, partial: true, roles: { cache: new Set() } });
  let fetched = null;
  w.guild.members.fetch = async (opts) => { fetched = opts; return real; };
  assert.equal(await rec(w), 'added');
  assert.deepEqual(fetched, { user: 'U', force: true });
  assert.equal(w.calls.length, 1);
});

test('reconcileMember: required role. A streaming member without it gets nothing; a stop removes the role regardless', async () => {
  const a = world({ streaming: true, requiredRoleId: 'R' });
  assert.equal(await rec(a), 'none');
  const b = world({ streaming: true, requiredRoleId: 'R', extraRoles: ['R'] });
  assert.equal(await rec(b), 'added');
  // stream stopped while the member also lost the required role: still removed
  const c = world({ roleHeld: true, requiredRoleId: 'R' });
  assert.equal(await rec(c), 'removed');
  // a deleted required role fails closed
  const d = world({ streaming: true, requiredRoleId: 'R', extraRoles: ['R'] });
  d.guild.roles.cache.delete('R');
  assert.equal(await rec(d), 'none');
});

test('reconcileMember: an unavailable guild is left alone (rule 20)', async () => {
  const w = world({ roleHeld: true });
  w.guild.available = false;
  assert.equal(await rec(w), 'none');
  assert.equal(w.calls.length, 0);
});

test('reconcileMember: never throws on a broken guild', async () => {
  assert.equal(await reconcileMember(null, 'U', { settings: {} }), 'failed');
  assert.equal(await reconcileMember({ id: 'G' }, 'U', { settings: { liveRoleId: 'L' } }), 'failed');
});

test('reconcileMember: flapping presence is coalesced to one running and one waiting call and converges', async () => {
  const w = world({ streaming: true });
  let release;
  const gate = new Promise((r) => { release = r; });
  let started;
  const startedP = new Promise((r) => { started = r; });
  const origAdd = w.member.roles.add;
  w.member.roles.add = async (id, reason) => { started(); await gate; return origAdd(id, reason); };

  const first = rec(w); // running (blocked inside roles.add)
  await startedP;
  w.guild.presences.cache.delete('U'); // stream stops while the add is in flight
  const second = rec(w); // queued behind the first
  const third = rec(w); // dropped: the queued run will read the newest presence
  assert.equal(await third, 'none');
  release();
  assert.equal(await first, 'added');
  assert.equal(await second, 'removed');
  assert.deepEqual(w.calls.map((c) => c[0]), ['add', 'remove']);
  assert.equal(w.member.roles.cache.has('L'), false, 'final state matches the newest presence');
});

test('reconcileMember: sync-sourced calls are never coalesced', async () => {
  const w = world({ roleHeld: true });
  const results = await Promise.all([rec(w, { source: 'sync' }), rec(w, { source: 'sync' })]);
  assert.deepEqual(results, ['removed', 'none']);
});

// ── countLosingHolders / loadMembers / syncGuild (offline paths) ──────────────

test('countLosingHolders counts non-bot holders who are not streaming or not eligible', () => {
  const w = world({ roleHeld: true });
  const bot = { id: 'B', user: { bot: true }, roles: { cache: new Set(['L']) } };
  const streamer = { id: 'T', user: { bot: false }, roles: { cache: new Set(['L']) } };
  w.role.members = new Map([['U', w.member], ['B', bot], ['T', streamer]]);
  w.guild.presences.cache.set('T', { activities: [{ type: 1 }] });
  assert.equal(countLosingHolders(w.guild, w.role, w.settings), 1);
  assert.equal(countLosingHolders(w.guild, w.role, { ...w.settings, liveRoleRequiredRoleId: 'R' }), 2, 'a deleted required role fails closed');
  assert.equal(countLosingHolders(null, null, null), 0, 'never throws');
});

test('loadMembers: a complete cache needs no fetch; too-large guilds use the cache', async () => {
  let fetches = 0;
  const complete = { id: 'G1', memberCount: 1, members: { cache: new Map([['U', {}]]), fetch: async () => { fetches++; } } };
  assert.equal(await loadMembers(complete), true);
  const big = { id: 'G2', memberCount: LIVEROLE.FULL_FETCH_MAX_MEMBERS + 1, members: { cache: new Map(), fetch: async () => { fetches++; } } };
  assert.equal(await loadMembers(big), false);
  assert.equal(fetches, 0);
  assert.equal(await loadMembers(null), false, 'never throws');
});

test('loadMembers: one full fetch per guild per interval', async () => {
  let fetches = 0;
  const guild = { id: 'G3', memberCount: 2, members: { cache: new Map(), fetch: async () => { fetches++; return new Map(); } } };
  assert.equal(await loadMembers(guild), true);
  assert.equal(await loadMembers(guild), false, 'throttled');
  assert.equal(fetches, 1);
});

test('syncGuild never throws on an unavailable or missing guild', async () => {
  assert.deepEqual(await syncGuild({ id: 'G', available: false }), { ok: false, code: 'error' });
  assert.deepEqual(await syncGuild(null), { ok: false, code: 'error' });
});

test('syncGuild reports unreachable settings instead of throwing', { skip: process.env.TEST_DATABASE_URL ? 'runs offline only (settings must fail to load)' : false }, async () => {
  const res = await syncGuild({ id: 'G-offline' });
  assert.deepEqual(res, { ok: false, code: 'settings' });
});

// ── 11. Payload ───────────────────────────────────────────────────────────────

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/liverole payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'liverole');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['set', 1], ['require', 1], ['enable', 1], ['disable', 1], ['status', 1]]);
  const sub = Object.fromEntries(json.options.map((o) => [o.name, o]));
  // `role` is optional because `new_role` can create the role instead (newTargets); runSet refuses when both are empty.
  assert.deepEqual(sub.set.options.map((o) => [o.name, o.type, !!o.required]), [['role', 8, false], ['new_role', 3, false]]);
  assert.deepEqual([sub.require.options[0].name, sub.require.options[0].type], ['role', 8]);
  assert.ok(!sub.require.options[0].required);
  assert.deepEqual([sub.disable.options[0].name, sub.disable.options[0].type], ['remove_roles', 5]);
  assert.ok(!sub.disable.options[0].required);
  assert.deepEqual(sub.enable.options ?? [], []);
  assert.deepEqual(sub.status.options ?? [], []);
  for (const o of json.options) assert.ok(o.description.length >= 1 && o.description.length <= 100);
  assert.ok(json.description.length <= 100);
  assert.ok(charCount(json) <= 550, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'liverole');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(cmd.autocomplete, undefined);
  assert.equal(cmd.allowInDMs, undefined);
  assert.equal(typeof cmd.execute, 'function');
});

// ── 12. Plugin declaration ────────────────────────────────────────────────────

test('plugin declaration', async () => {
  assert.equal(plugin.id, 'liverole');
  assert.equal(plugin.module, 'liverole');
  assert.deepEqual(plugin.hooks.map((h) => ({ event: h.event, id: h.id })), [
    { event: 'presenceUpdate', id: 'liverole' },
    { event: 'guildReady', id: 'liverole' },
    { event: 'guildMemberUpdate', id: 'liverole' },
  ]);
  for (const h of plugin.hooks) {
    assert.ok(HOOK_ORDER[h.event].includes(h.id), `${h.event}:${h.id} is in HOOK_ORDER`);
    assert.notEqual(h.always, true);
    assert.equal(typeof h.run, 'function');
  }
  assert.equal(plugin.hooks[1].timeoutMs, 120_000);
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs, []);
  assert.deepEqual(plugin.provides, {});
  const { errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('liverole')), []);
});

test('presenceUpdate hook filter: sync, change-only', () => {
  const filter = plugin.hooks[0].filter;
  assert.equal(filter({ guildId: 'no-memo', oldPresence: null, newPresence: S }), true);
  assert.equal(filter({ guildId: 'no-memo', oldPresence: S, newPresence: S }), false);
  assert.equal(filter({ guildId: 'no-memo', oldPresence: P, newPresence: P }), false);
});

test('hooks do nothing without a live role in the settings', async () => {
  const w = world({ roleHeld: true });
  const ctx = (settings) => ({
    guild: w.guild, reason: 'startup', newPresence: { userId: 'U' }, member: w.member, getSettings: async () => settings,
  });
  await plugin.hooks[0].run(ctx({ liveRoleEnabled: false, liveRoleId: 'L' }));
  await plugin.hooks[0].run(ctx({ liveRoleEnabled: true, liveRoleId: null }));
  await plugin.hooks[0].run(ctx(null));
  await plugin.hooks[1].run(ctx(null));
  assert.equal(w.calls.length, 0);
});

test('the presence hook reconciles the member from the settings the runner loaded', async () => {
  const w = world({ roleHeld: true });
  await plugin.hooks[0].run({
    guild: w.guild, newPresence: { userId: 'U' }, member: w.member, getSettings: async () => w.settings,
  });
  assert.deepEqual(w.calls, [['remove', 'L', REASONS.presenceRemove]]);
});

// ── 13. Static source checks ──────────────────────────────────────────────────

test('static source checks on both files', () => {
  for (const file of ['../src/plugins/liverole.js', '../src/commands/admin/liverole.js']) {
    const text = readFileSync(new URL(file, import.meta.url), 'utf8');
    assert.ok(!text.includes('roles.set('), `${file}: roles.set(`);
    assert.ok(!text.includes('roles.add(['), `${file}: roles.add([`);
    assert.ok(!text.includes('roles.remove(['), `${file}: roles.remove([`);
    assert.ok(!text.includes('setCustomId('), `${file}: setCustomId(`);
    assert.ok(!text.includes('customId:'), `${file}: customId:`);
    assert.ok(!/console\./.test(text), `${file}: console.`);
    assert.ok(!text.includes('cache.del('), `${file}: cache.del(`);
    assert.ok(!/from\s+['"][^'"]*(prisma|database\/client)/.test(text), `${file}: prisma import`);
  }
});

// ── Command: failure paths with a fake interaction ────────────────────────────

function fakeInteraction(sub) {
  const calls = { defer: 0, edits: [] };
  return {
    calls,
    guildId: 'G-offline-cmd',
    guild: { id: 'G-offline-cmd' },
    user: { id: 'u1' },
    deferred: false,
    replied: false,
    options: { getSubcommand: () => sub, getRole: () => null, getBoolean: () => null },
    deferReply: async () => { calls.defer++; },
    editReply: async (p) => { calls.edits.push(p); return {}; },
  };
}

test('every subcommand defers, then answers "Settings Unavailable" publicly without pinging when settings cannot load', { skip: process.env.TEST_DATABASE_URL ? 'runs offline only (settings must fail to load)' : false }, async () => {
  const subs = ['set', 'require', 'enable', 'disable', 'status'];
  const interactions = subs.map((sub) => ({ sub, i: fakeInteraction(sub) }));
  await Promise.all(interactions.map(({ i }) => cmd.execute(i))); // concurrent: one shared settings load
  for (const { sub, i } of interactions) {
    assert.equal(i.calls.defer, 1, sub);
    assert.equal(i.calls.edits.length, 1, sub);
    const p = i.calls.edits[0];
    assert.deepEqual(p.allowedMentions, { parse: [] }, sub);
    assert.deepEqual(p.components, [], sub);
    assert.match(p.embeds[0].data.title, /Settings Unavailable/, sub);
  }
});

// ── Command flows against a real database (TEST_DATABASE_URL, migrated) ───────
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/liverole.test.js
// Fake guild and interaction objects; settings go through the real settingsService (Postgres), role calls hit spies.

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';
const { Collection } = await import('discord.js');
const { EventEmitter } = await import('node:events');
const { updateSettings, getSettingsSafe } = await import('../src/services/settingsService.js');

let dbSeq = 0;
const dbGuildId = () => {
  const id = `95503200000${String(++dbSeq).padStart(7, '0')}`; // unique to this package (port 55032)
  DB_GUILDS.push(id);
  return id;
};
function mkWorld(guildId) {
  const calls = [];
  const members = new Collection();
  const roles = new Collection();
  const presences = new Collection();
  const me = { permissions: { has: () => true }, roles: { highest: { position: 10 } } };
  const guild = {
    id: guildId,
    ownerId: 'OWNER',
    roles: { cache: roles },
    members: { me, cache: members, fetch: async () => members },
    presences: { cache: presences },
    get memberCount() { return members.size; },
  };
  const addRole = (id, { position = 1, managed = false, dangerous = false } = {}) => {
    const role = {
      id, position, managed, permissions: { has: () => dangerous }, get members() { return members.filter((m) => m.roles.cache.has(id)); },
    };
    roles.set(id, role);
    return role;
  };
  const addMember = (id, { roleIds = [], streaming = false, bot = false } = {}) => {
    const ids = new Set(roleIds);
    const member = {
      id,
      partial: false,
      user: { bot },
      roles: {
        cache: ids,
        add: async (rid, reason) => { calls.push(['add', id, rid, reason]); ids.add(rid); },
        remove: async (rid, reason) => { calls.push(['remove', id, rid, reason]); ids.delete(rid); },
      },
    };
    members.set(id, member);
    if (streaming) presences.set(id, { userId: id, activities: [{ type: 1, name: 'Twitch' }], member });
    return member;
  };
  return { guild, calls, addRole, addMember, members, presences, me };
}

/** A fake slash interaction. answer: 'yes' | 'no' | null (timeout) for an awaitConfirm dialog. */
function mkInteraction(w, sub, { role = null, bool = null, answer = null } = {}) {
  const edits = [];
  let yesId = null;
  let noId = null;
  const message = {
    id: 'm1',
    edit: async () => {},
    createMessageComponentCollector: () => {
      const col = new EventEmitter();
      setTimeout(() => {
        const customId = answer === 'yes' ? yesId : (answer === 'no' ? noId : null);
        if (customId) col.emit('collect', { customId, update: async () => {} });
        col.emit('end');
      }, 5);
      return col;
    },
  };
  const interaction = {
    guild: w.guild,
    guildId: w.guild.id,
    user: { id: 'admin' },
    member: { id: 'admin', roles: { highest: { position: 20 } } },
    deferred: false,
    replied: false,
    options: { getSubcommand: () => sub, getSubcommandGroup: () => null, get: () => (role ? { id: role } : null), getString: () => null, getRole: () => (role ? { id: role } : null), getBoolean: () => bool },
    deferReply: async () => { interaction.deferred = true; interaction.ephemeral = false; },
    editReply: async (p) => {
      edits.push(p);
      const row = p.components?.[0];
      if (row?.components?.length === 2) {
        yesId = row.components[0].data.custom_id;
        noId = row.components[1].data.custom_id;
      }
      return message;
    },
  };
  interaction.edits = edits;
  interaction.last = () => edits.at(-1);
  interaction.title = () => interaction.last()?.embeds?.[0]?.data?.title ?? '';
  interaction.text = () => interaction.last()?.embeds?.[0]?.data?.description ?? '';
  return interaction;
}

const row = (guildId) => prisma.guildSettings.findUnique({ where: { guildId } });

test('/liverole set: validation refuses @everyone, managed, dangerous and above-bot roles and writes nothing', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole(w.guild.id); // @everyone
  w.addRole('M', { managed: true });
  w.addRole('D', { dangerous: true });
  w.addRole('H', { position: 30 }); // above the bot (10) and the executor (20)
  for (const [roleId, re] of [[w.guild.id, /Invalid Role/], ['M', /Cannot Use That Role/], ['D', /Cannot Use That Role/], ['H', /Cannot Use That Role/], ['nope', /Cannot Use That Role/]]) {
    const i = mkInteraction(w, 'set', { role: roleId });
    await cmd.execute(i);
    assert.match(i.title(), re, roleId);
    assert.deepEqual(i.last().allowedMentions, { parse: [] });
  }
  const dangerous = mkInteraction(w, 'set', { role: 'D' });
  await cmd.execute(dangerous);
  assert.match(dangerous.text(), /dangerous permissions/);
  assert.equal((await row(w.guild.id))?.liveRoleId ?? null, null);
});

test('/liverole set: a dedicated role is saved; the module stays off and nothing is synced', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  const i = mkInteraction(w, 'set', { role: 'L' });
  await cmd.execute(i);
  assert.equal(i.title(), '✅  Live Role Set');
  assert.match(i.text(), /<@&L>/);
  assert.match(i.text(), /live role is \*\*off\*\*/);
  assert.equal((await row(w.guild.id)).liveRoleId, 'L');
  assert.equal((await row(w.guild.id)).liveRoleEnabled, false);
  assert.equal(w.calls.length, 0);
  // the same role again changes nothing
  const again = mkInteraction(w, 'set', { role: 'L' });
  await cmd.execute(again);
  assert.match(again.title(), /No Change/);
});

test('/liverole set new_role: creates a role with no permissions and saves it; neither picker nor name is refused', { skip: skipDb }, async () => {
  const { CommandInteractionOptionResolver, PermissionsBitField, PermissionFlagsBits } = await import('discord.js');
  const w = mkWorld(dbGuildId());
  w.addRole(w.guild.id); // @everyone
  const created = [];
  w.guild.roles.create = async (o) => { created.push(o); return Object.assign(w.addRole('NEW'), { name: o.name }); };
  // A real resolver, so the new_role → role substitution runs exactly as in Discord.
  const withOptions = (opts) => Object.assign(mkInteraction(w, 'set'), {
    commandName: 'liverole',
    user: { id: 'admin', username: 'admin' },
    memberPermissions: new PermissionsBitField(PermissionFlagsBits.ManageRoles),
    options: new CommandInteractionOptionResolver({}, [{ name: 'set', type: 1, options: opts }], null),
  });

  const i = withOptions([{ name: 'new_role', type: 3, value: 'Live' }]);
  await cmd.execute(i);
  assert.equal(i.title(), '✅  Live Role Set');
  assert.match(i.text(), /<@&NEW>/);
  assert.deepEqual(created.map((o) => [o.name, o.permissions]), [['Live', []]]);
  assert.equal((await row(w.guild.id)).liveRoleId, 'NEW');

  const none = withOptions([]);
  await cmd.execute(none);
  assert.match(none.title(), /Missing Role/);
  assert.equal(created.length, 1);
});

test('/liverole set: the dedicated-role guard refuses > 50 holders and confirms 1-50', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('BIG');
  for (let k = 0; k < LIVEROLE.SET_MAX_STRIP + 1; k++) w.addMember(`m${k}`, { roleIds: ['BIG'] });
  const refused = mkInteraction(w, 'set', { role: 'BIG' });
  await cmd.execute(refused);
  assert.match(refused.title(), /Use a Dedicated Role/);
  assert.match(refused.text(), /\*\*51\*\*/);
  assert.equal((await row(w.guild.id))?.liveRoleId ?? null, null);

  const w2 = mkWorld(dbGuildId());
  w2.addRole('SMALL');
  w2.addMember('a', { roleIds: ['SMALL'] });
  w2.addMember('b', { roleIds: ['SMALL'], streaming: true }); // streaming holders do not count
  const declined = mkInteraction(w2, 'set', { role: 'SMALL', answer: 'no' });
  await cmd.execute(declined);
  assert.equal((await row(w2.guild.id))?.liveRoleId ?? null, null, 'cancel writes nothing');
  assert.ok(declined.edits.some((e) => e.embeds?.[0]?.data?.description?.includes('**1** member(s)')), 'the confirm dialog names the count');

  const timedOut = mkInteraction(w2, 'set', { role: 'SMALL' });
  await cmd.execute(timedOut);
  assert.equal((await row(w2.guild.id))?.liveRoleId ?? null, null, 'a timeout writes nothing');

  const accepted = mkInteraction(w2, 'set', { role: 'SMALL', answer: 'yes' });
  await cmd.execute(accepted);
  assert.equal((await row(w2.guild.id)).liveRoleId, 'SMALL');
  assert.equal(accepted.title(), '✅  Live Role Set');
});

test('/liverole set while enabled syncs and reports the previous role without touching it', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('OLD');
  w.addRole('NEW');
  w.addMember('streamer', { streaming: true, roleIds: ['OLD'] });
  await updateSettings(w.guild.id, { liveRoleId: 'OLD', liveRoleEnabled: true });
  const i = mkInteraction(w, 'set', { role: 'NEW' });
  await cmd.execute(i);
  assert.equal((await row(w.guild.id)).liveRoleId, 'NEW');
  assert.deepEqual(w.calls.map((c) => c.slice(0, 3)), [['add', 'streamer', 'NEW']]);
  assert.match(i.text(), /Synced: gave it to \*\*1\*\*/);
  assert.match(i.text(), /previous live role <@&OLD>/);
  assert.ok(w.members.get('streamer').roles.cache.has('OLD'), 'the old role is left alone');
});

test('/liverole require: sets, clears, rejects the live role, and resyncs while enabled', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  w.addRole('REQ', { managed: true, position: 99 }); // managed and above the bot is fine for a required role
  w.addMember('ok', { streaming: true, roleIds: ['REQ'] });
  w.addMember('no', { streaming: true, roleIds: ['L'] }); // holds the live role but lacks the requirement
  await updateSettings(w.guild.id, { liveRoleId: 'L', liveRoleEnabled: true });

  const same = mkInteraction(w, 'require', { role: 'L' });
  await cmd.execute(same);
  assert.match(same.title(), /Invalid Role/);

  const none = mkInteraction(w, 'require');
  await cmd.execute(none);
  assert.match(none.title(), /No Change/);

  const set = mkInteraction(w, 'require', { role: 'REQ' });
  await cmd.execute(set);
  assert.equal(set.title(), '✅  Live Role Requirement Updated');
  assert.equal((await row(w.guild.id)).liveRoleRequiredRoleId, 'REQ');
  assert.deepEqual(w.calls.map((c) => c.slice(0, 3)).sort(), [['add', 'ok', 'L'], ['remove', 'no', 'L']]);

  const cleared = mkInteraction(w, 'require');
  await cmd.execute(cleared);
  assert.equal((await row(w.guild.id)).liveRoleRequiredRoleId, null);
  assert.match(cleared.text(), /Everyone who streams can get the live role\./);

  const everyone = mkInteraction(w, 'require', { role: w.guild.id });
  await cmd.execute(everyone);
  assert.match(everyone.title(), /No Change/, '@everyone is stored as none');
});

test('/liverole enable: refuses without a usable role, confirms stale holders, then syncs; a second run resyncs', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  const noRole = mkInteraction(w, 'enable');
  await cmd.execute(noRole);
  assert.match(noRole.title(), /No Live Role Set/);

  await updateSettings(w.guild.id, { liveRoleId: 'GONE' });
  const deleted = mkInteraction(w, 'enable');
  await cmd.execute(deleted);
  assert.match(deleted.title(), /Live Role Missing/);
  assert.equal((await row(w.guild.id)).liveRoleId, 'GONE', 'a missing role is never cleared by the command');

  w.addRole('L');
  w.addMember('streamer', { streaming: true });
  w.addMember('stale', { roleIds: ['L'] });
  await updateSettings(w.guild.id, { liveRoleId: 'L' });

  const declined = mkInteraction(w, 'enable', { answer: 'no' });
  await cmd.execute(declined);
  assert.equal((await row(w.guild.id)).liveRoleEnabled, false);
  assert.equal(w.calls.length, 0);

  const yes = mkInteraction(w, 'enable', { answer: 'yes' });
  await cmd.execute(yes);
  assert.equal(yes.title(), '✅  Live Role Enabled');
  assert.equal((await row(w.guild.id)).liveRoleEnabled, true);
  assert.deepEqual(w.calls.map((c) => c.slice(0, 3)).sort(), [['add', 'streamer', 'L'], ['remove', 'stale', 'L']]);

  const resync = mkInteraction(w, 'enable');
  await cmd.execute(resync);
  assert.equal(resync.title(), '✅  Live Role Already On — Resynced');
  assert.match(resync.text(), /Everyone is already in sync\./);
});

test('/liverole enable refuses a role that gained dangerous permissions', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L', { dangerous: true });
  await updateSettings(w.guild.id, { liveRoleId: 'L' });
  const i = mkInteraction(w, 'enable');
  await cmd.execute(i);
  assert.match(i.title(), /Cannot Use That Role/);
  assert.equal((await row(w.guild.id)).liveRoleEnabled, false);
});

test('/liverole disable removes the role from every holder (streaming ones too) unless remove_roles is false', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  w.addMember('streamer', { streaming: true, roleIds: ['L'] });
  w.addMember('other', { roleIds: ['L'] });
  w.addMember('bot', { roleIds: ['L'], bot: true });
  await updateSettings(w.guild.id, { liveRoleId: 'L', liveRoleEnabled: true });

  const keep = mkInteraction(w, 'disable', { bool: false });
  await cmd.execute(keep);
  assert.equal(keep.title(), '✅  Live Role Disabled');
  assert.equal((await row(w.guild.id)).liveRoleEnabled, false);
  assert.equal(w.calls.length, 0);
  assert.match(keep.text(), /keep it/);

  const clear = mkInteraction(w, 'disable');
  await cmd.execute(clear);
  assert.equal(clear.title(), '✅  Live Role Already Off');
  assert.deepEqual(w.calls.map((c) => c.slice(0, 3)).sort(), [['remove', 'other', 'L'], ['remove', 'streamer', 'L']]);
  assert.ok(w.calls.every((c) => c[3] === REASONS.clear));
  assert.ok(w.members.get('bot').roles.cache.has('L'), 'bots are never touched');
  assert.match(clear.text(), /Removed the live role from \*\*2\*\* member\(s\)\./);
  assert.equal((await row(w.guild.id)).liveRoleId, 'L', 'the role and requirement stay saved');
});

test('/liverole status is cache-only and flags problems', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  w.addMember('holder', { roleIds: ['L'], streaming: true });
  w.addMember('idle', { roleIds: ['L'] });
  await updateSettings(w.guild.id, { liveRoleId: 'L', liveRoleEnabled: false, liveRoleRequiredRoleId: 'DELETED' });
  const i = mkInteraction(w, 'status');
  await cmd.execute(i);
  const embed = i.last().embeds[0].data;
  assert.equal(embed.title, '🔴 Live Role');
  const field = (name) => embed.fields.find((f) => f.name === name)?.value;
  assert.match(field('Status'), /Off/);
  assert.equal(field('Live role'), '<@&L>');
  assert.match(field('Required role'), /Deleted/);
  assert.match(field('Live now'), /\*\*2\*\* member\(s\) have the live role · \*\*1\*\* member\(s\) are streaming/);
  assert.match(field('Live now'), /<@holder> · Twitch/);
  assert.match(field('Live now'), /<@idle> · not streaming/);
  assert.match(field('Problems'), /required role was deleted/);
  assert.match(field('Problems'), /still have the live role/);
  assert.ok(embed.fields.length <= 5);
  for (const f of embed.fields) assert.ok(f.value.length <= 1024);
  assert.equal(w.calls.length, 0);
});

test('/liverole status on a fresh guild', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  const i = mkInteraction(w, 'status');
  await cmd.execute(i);
  const fields = i.last().embeds[0].data.fields;
  assert.equal(fields.find((f) => f.name === 'Live role').value, 'Not set — `/liverole set`');
  assert.equal(fields.find((f) => f.name === 'Live now').value, '—');
  assert.equal(fields.some((f) => f.name === 'Problems'), false);
});

test('syncGuild: adds first, caps the edits, removes non-streamers, honours the required role', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  w.addRole('REQ');
  const cap = LIVEROLE.SYNC_MAX_CHANGES;
  for (let k = 0; k < 3; k++) w.addMember(`s${k}`, { streaming: true, roleIds: ['REQ'] });
  w.addMember('lacks', { streaming: true }); // streams but lacks the requirement
  for (let k = 0; k < cap; k++) w.addMember(`h${k}`, { roleIds: ['L'] }); // stale holders
  await updateSettings(w.guild.id, { liveRoleId: 'L', liveRoleEnabled: true, liveRoleRequiredRoleId: 'REQ' });

  const res = await syncGuild(w.guild, { mode: 'sync', reason: 'test' });
  assert.equal(res.ok, true);
  assert.equal(res.added, 3);
  assert.equal(res.removed, cap - 3, 'adds are processed first, then removals up to the cap');
  assert.equal(res.deferred, 3);
  assert.equal(res.complete, true);
  assert.ok(!w.members.get('lacks').roles.cache.has('L'));
  assert.deepEqual(w.calls.slice(0, 3).map((c) => c[0]), ['add', 'add', 'add']);

  const next = await syncGuild(w.guild, { mode: 'sync', reason: 'test' });
  assert.equal(next.removed, 3);
  assert.equal(next.deferred, 0);

  await updateSettings(w.guild.id, { liveRoleEnabled: false });
  assert.deepEqual(await syncGuild(w.guild, { mode: 'sync' }), { ok: false, code: 'off' });
  assert.equal((await syncGuild(w.guild, { mode: 'clear' })).ok, true, 'clear mode works while the module is off');
});

test('syncGuild: result codes for a missing or unmanageable role, and dangerous roles block adds only', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  assert.deepEqual(await syncGuild(w.guild), { ok: false, code: 'off' });
  await updateSettings(w.guild.id, { liveRoleEnabled: true });
  assert.deepEqual(await syncGuild(w.guild), { ok: false, code: 'no-role' });
  await updateSettings(w.guild.id, { liveRoleId: 'L' });
  assert.deepEqual(await syncGuild(w.guild), { ok: false, code: 'role-missing' });
  assert.equal((await getSettingsSafe(w.guild.id)).liveRoleId, 'L', 'a missing role is never cleared');

  const role = w.addRole('L', { position: 99 });
  const blocked = await syncGuild(w.guild);
  assert.equal(blocked.code, 'cannot-manage');
  assert.match(blocked.reason, /above my highest role/);
  role.position = 1;

  w.addMember('streamer', { streaming: true });
  w.addMember('idle', { roleIds: ['L'] });
  role.permissions.has = () => true; // gained dangerous permissions
  const res = await syncGuild(w.guild);
  assert.equal(res.ok, true);
  assert.equal(res.added, 0);
  assert.equal(res.removed, 1);
  assert.match(res.addsBlocked, /dangerous permissions/);
});

test('syncGuild: a streaming user missing from the member cache is fetched, not skipped', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('L');
  const real = w.addMember('uncached', { streaming: true });
  w.members.delete('uncached');
  w.guild.members.fetch = async (opts) => {
    if (opts?.user === 'uncached') { w.members.set('uncached', real); return real; }
    return w.members;
  };
  await updateSettings(w.guild.id, { liveRoleId: 'L', liveRoleEnabled: true });
  const res = await syncGuild(w.guild);
  assert.equal(res.added, 1);
  assert.ok(real.roles.cache.has('L'));
});

test('reconcileMember: a presence decision made on stale settings is dropped after /liverole set or disable', { skip: skipDb }, async () => {
  const w = mkWorld(dbGuildId());
  w.addRole('OLD');
  w.addRole('NEW');
  w.addMember('U', { streaming: true });
  const stale = { liveRoleEnabled: true, liveRoleId: 'OLD', liveRoleRequiredRoleId: null };
  await updateSettings(w.guild.id, { liveRoleId: 'NEW', liveRoleEnabled: true }); // memo now says NEW
  assert.equal(await reconcileMember(w.guild, 'U', { mode: 'sync', settings: stale, source: 'presence' }), 'none');
  assert.equal(w.calls.length, 0);
  await updateSettings(w.guild.id, { liveRoleEnabled: false });
  const current = { liveRoleEnabled: true, liveRoleId: 'NEW', liveRoleRequiredRoleId: null };
  assert.equal(await reconcileMember(w.guild, 'U', { mode: 'sync', settings: current, source: 'presence' }), 'none');
  assert.equal(w.calls.length, 0);
  // sync-sourced calls are not affected by the memo
  assert.equal(await reconcileMember(w.guild, 'U', { mode: 'sync', settings: current, source: 'sync' }), 'added');
});

test('guildMemberUpdate hook: losing or gaining the required role while streaming re-checks the live role', async () => {
  const hook = plugin.hooks.find((h) => h.event === 'guildMemberUpdate');
  const w = world({ roleHeld: true, streaming: true, requiredRoleId: 'R' }); // streaming, holds L, no longer has R
  const ctx = (oldHadRequired) => ({
    guild: w.guild, getSettings: async () => w.settings, oldKnown: true,
    oldMember: { roles: { cache: new Set(oldHadRequired ? ['R'] : []) } }, newMember: w.member,
  });
  assert.equal(hook.filter(ctx(true)), true);
  assert.equal(hook.filter({ ...ctx(true), oldKnown: false }), false, 'an uncached old member cannot be diffed');
  await hook.run(ctx(true));
  assert.deepEqual(w.calls, [['remove', 'L', REASONS.syncRemove]], 'lost the required role: the live role goes');
  await hook.run(ctx(false));
  assert.equal(w.calls.length, 1, 'no change to the required role: nothing to do');
  w.member.roles.cache.add('R');
  await hook.run(ctx(false));
  assert.deepEqual(w.calls[1], ['add', 'L', REASONS.syncAdd], 'gained it while streaming: the live role comes back');
});
