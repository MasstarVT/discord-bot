// Voice Roles tests. Offline (no Discord, Postgres or Redis): the rules (which roles a member should have), the role changes
// against fake guilds and members whose role cache lags behind (like discord.js), the startup / sync reconcile, the cleanup
// hooks against an in-memory GuildSettings table, the hook filters, the plugin declaration and the /voicerole payload and replies.
import { test, after, beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { PermissionFlagsBits, ChannelType } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/voiceRoleService.js');
const vr = await import('../src/plugins/voice-roles.js');
const cmd = await import('../src/commands/admin/voicerole.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS, LIMITS } = await import('../src/config/constants.js');
const { invalidateSettings, getSettings } = await import('../src/services/settingsService.js');

const {
  VOICE_ROLES, linksOf, anyRolesOf, hasVoiceRoles, managedRoleIds, voiceOf, desiredRoleIds, applyMember, reconcileGuild,
  isReconciling, resetState, patchDropChannel, patchDropRole,
} = svc;
const {
  plugin, voiceFilter, channelDeleteFilter, roleDeleteFilter, onVoiceStateUpdate, onGuildReady, onChannelDelete, onRoleDelete,
  __startupSyncsIdle,
} = vr;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory GuildSettings ───────────────────────────────────────────────────

const settingsRows = new Map();
const realGuildSettings = prisma.guildSettings;
const realTransaction = prisma.$transaction;
const settingsStub = {
  findUnique: async ({ where }) => (settingsRows.has(where.guildId) ? structuredClone(settingsRows.get(where.guildId)) : null),
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsRows.set(where.guildId, row);
    return structuredClone(row);
  },
  update: async ({ where, data }) => {
    const row = { ...settingsRows.get(where.guildId), ...data };
    settingsRows.set(where.guildId, row);
    return structuredClone(row);
  },
};
prisma.guildSettings = settingsStub;
prisma.$transaction = async (fn) => fn({ $executeRaw: async () => 0, $queryRaw: async () => [], guildSettings: settingsStub });

after(async () => {
  prisma.guildSettings = realGuildSettings;
  prisma.$transaction = realTransaction;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

beforeEach(() => {
  resetState();
});

const BASE_SETTINGS = Object.freeze({
  voiceRolesEnabled: true,
  voiceRoleLinks: [],
  voiceRoleAnyRoleIds: [],
  voiceRoleIgnoreAfk: true,
  embedColor: null,
});
const SETTINGS = (over = {}) => ({ ...BASE_SETTINGS, voiceRoleLinks: [], voiceRoleAnyRoleIds: [], ...over });

/** Seeds the guild's GuildSettings row and drops the memo, so the next read loads this row. */
async function seedSettings(guildId, over = {}) {
  settingsRows.set(guildId, { guildId, ...SETTINGS(over) });
  await invalidateSettings(guildId);
  return settingsRows.get(guildId);
}

// ── Fakes ─────────────────────────────────────────────────────────────────────

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const discordErr = (code) => Object.assign(new Error(`Discord error ${code}`), { code });

function fakeRole(id, { position = 1, managed = false, perms = [] } = {}) {
  return {
    id, name: id, position, managed,
    permissions: { has: (flag) => perms.some((k) => PermissionFlagsBits[k] === flag) },
  };
}

let guildCounter = 0;
/** A guild with voice channels VA and VB (in category CAT), VC (loose), AFK (the AFK channel) and a text channel TXT. */
function fakeGuild({ botPosition = 10, botPerms = ['ManageRoles'], extraRoles = [] } = {}) {
  const guild = {
    id: `VG${++guildCounter}`, ownerId: 'OWNER', afkChannelId: 'AFK',
    roles: { cache: new Map() },
    channels: { cache: new Map() },
    voiceStates: { cache: new Map() },
    members: {
      cache: new Map(),
      me: {
        permissions: { has: (flag) => botPerms.some((k) => PermissionFlagsBits[k] === flag) },
        roles: { highest: { position: botPosition } },
      },
    },
  };
  const roles = [
    fakeRole(guild.id), fakeRole('rA'), fakeRole('rB'), fakeRole('rC'), fakeRole('rAny'), fakeRole('rCat'),
    fakeRole('rMANAGED', { managed: true }), fakeRole('rHIGH', { position: 20 }), fakeRole('rMID', { position: 8 }),
    fakeRole('rADMIN', { perms: ['Administrator'] }), ...extraRoles,
  ];
  for (const r of roles) guild.roles.cache.set(r.id, r);
  const channels = [
    { id: 'VA', type: ChannelType.GuildVoice, parentId: 'CAT' },
    { id: 'VB', type: ChannelType.GuildVoice, parentId: 'CAT' },
    { id: 'VC', type: ChannelType.GuildVoice, parentId: null },
    { id: 'ST', type: ChannelType.GuildStageVoice, parentId: null },
    { id: 'AFK', type: ChannelType.GuildVoice, parentId: null },
    { id: 'CAT', type: ChannelType.GuildCategory, parentId: null },
    { id: 'TXT', type: ChannelType.GuildText, parentId: null },
  ];
  for (const c of channels) guild.channels.cache.set(c.id, c);
  return guild;
}

/**
 * A member whose role calls change what Discord holds (`truth`) but NOT `roles.cache`, exactly like discord.js until the
 * gateway echo arrives (`echo()`). Calls are recorded; `delay` makes each call take that long; `fail(op, id)` may throw.
 */
function fakeMember(guild, id, { roles = [], bot = false, delay = 0 } = {}) {
  const member = {
    id, guild, user: { id, bot }, calls: [], truth: new Set(roles), delay, fail: null, inflight: 0, maxInflight: 0,
    roles: { cache: new Map([[guild.id, guild.roles.cache.get(guild.id)], ...roles.map((r) => [r, guild.roles.cache.get(r) ?? { id: r }])]) },
    echo() {
      member.roles.cache = new Map([[guild.id, guild.roles.cache.get(guild.id)], ...[...member.truth].map((r) => [r, { id: r }])]);
    },
  };
  const op = (name) => async (roleId, reason) => {
    member.inflight++;
    member.maxInflight = Math.max(member.maxInflight, member.inflight);
    try {
      if (member.delay) await sleep(member.delay);
      member.calls.push({ op: name, id: roleId, reason });
      const err = member.fail?.(name, roleId);
      if (err) throw err;
      if (name === 'add') member.truth.add(roleId); else member.truth.delete(roleId);
    } finally {
      member.inflight--;
    }
  };
  member.roles.add = op('add');
  member.roles.remove = op('remove');
  guild.members.cache.set(id, member);
  return member;
}

/** Puts the member in a voice channel (null: out of voice), in the guild's voice-state cache. */
function setVoice(guild, member, channelId) {
  if (!channelId) { guild.voiceStates.cache.delete(member.id); return; }
  guild.voiceStates.cache.set(member.id, { id: member.id, channelId, member, channel: guild.channels.cache.get(channelId) });
}

const LINKS = [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'VB', roleId: 'rB' }];
const truthOf = (m) => [...m.truth].sort();

// ── Settings shapes and the rules ─────────────────────────────────────────────

test('settings shapes: junk rows are ignored, duplicates dropped, managed roles are links plus the any-list', () => {
  const settings = {
    voiceRoleLinks: [
      { channelId: 'VA', roleId: 'rA' }, { channelId: 'VA', roleId: 'rA' }, { channelId: 'VB' }, null, 'x', { roleId: 'rB' },
      { channelId: 'CAT', roleId: 'rCat' },
    ],
    voiceRoleAnyRoleIds: ['rAny', 'rAny', 5, '', 'rA'],
  };
  assert.deepEqual(linksOf(settings), [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'CAT', roleId: 'rCat' }]);
  assert.deepEqual(anyRolesOf(settings), ['rAny', 'rA']);
  assert.deepEqual([...managedRoleIds(settings)].sort(), ['rA', 'rAny', 'rCat']);
  assert.equal(hasVoiceRoles(settings), true);
  assert.equal(hasVoiceRoles({ voiceRoleLinks: [], voiceRoleAnyRoleIds: [] }), false);
  assert.equal(hasVoiceRoles(null), false);
  assert.deepEqual(linksOf(null), []);
  assert.equal(managedRoleIds({}).size, 0);
});

test('desired roles: channel, category, any-list, not in voice, AFK', () => {
  const guild = fakeGuild();
  const settings = SETTINGS({
    voiceRoleLinks: [...LINKS, { channelId: 'CAT', roleId: 'rCat' }, { channelId: 'ST', roleId: 'rC' }],
    voiceRoleAnyRoleIds: ['rAny'],
  });
  const at = (channelId) => voiceOf({ channelId }, guild);
  const want = (voice, s = settings) => [...desiredRoleIds(s, guild, voice)].sort();
  assert.deepEqual(want(at('VA')), ['rA', 'rAny', 'rCat'], 'its own link, its category link and the any-list');
  assert.deepEqual(want(at('VB')), ['rAny', 'rB', 'rCat']);
  assert.deepEqual(want(at('VC')), ['rAny'], 'a loose channel only gets the any-list');
  assert.deepEqual(want(at('ST')), ['rAny', 'rC'], 'a stage channel can be linked');
  assert.deepEqual(want(null), [], 'not in voice: nothing');
  assert.deepEqual(want(at('AFK')), [], 'the AFK channel gets nothing by default');
  assert.deepEqual(want(at('AFK'), { ...settings, voiceRoleIgnoreAfk: false }), ['rAny'], 'unless AFK is not ignored');
  assert.deepEqual(want(at('AFK'), { ...settings, voiceRoleIgnoreAfk: undefined }), [], 'a missing flag counts as on');
  assert.deepEqual(voiceOf({ channelId: 'VA' }, guild), { channelId: 'VA', parentId: 'CAT' });
  assert.equal(voiceOf({ channelId: null }, guild), null);
  assert.equal(voiceOf(null, guild), null);
  // A server with no AFK channel: nothing is special.
  const noAfk = { ...guild, afkChannelId: null };
  assert.deepEqual(want(at('AFK'), settings).length, 0);
  assert.deepEqual([...desiredRoleIds(settings, noAfk, at('AFK'))], ['rAny']);
});

// ── Role changes ──────────────────────────────────────────────────────────────

test('join, move, leave: roles follow the voice state, one role per call with an audit reason', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS, voiceRoleAnyRoleIds: ['rAny'] });
  const m = fakeMember(guild, 'U1', { roles: ['rC'] }); // rC is not managed: never touched

  setVoice(guild, m, 'VA');
  let res = await applyMember(guild, m, settings);
  assert.equal(res.status, 'ok');
  assert.deepEqual(res.added.sort(), ['rA', 'rAny']);
  assert.deepEqual(m.calls.map((c) => c.op), ['add', 'add'], 'single-role calls');
  assert.ok(m.calls.every((c) => typeof c.id === 'string' && c.reason === VOICE_ROLES.ADD_REASON));
  assert.deepEqual(truthOf(m), ['rA', 'rAny', 'rC']);
  m.echo();

  m.calls.length = 0;
  setVoice(guild, m, 'VB');
  res = await applyMember(guild, m, settings);
  assert.deepEqual(res.added, ['rB']);
  assert.deepEqual(res.removed, ['rA']);
  assert.deepEqual(m.calls.map((c) => c.op), ['add', 'remove'], 'adds first, then removes');
  assert.equal(m.calls[1].reason, VOICE_ROLES.REMOVE_REASON);
  assert.deepEqual(truthOf(m), ['rAny', 'rB', 'rC']);
  m.echo();

  m.calls.length = 0;
  setVoice(guild, m, 'VB');
  res = await applyMember(guild, m, settings);
  assert.deepEqual(m.calls, [], 'nothing differs: no calls');

  setVoice(guild, m, null);
  res = await applyMember(guild, m, settings);
  assert.deepEqual(res.removed.sort(), ['rAny', 'rB']);
  assert.deepEqual(truthOf(m), ['rC'], 'unmanaged roles stay');
  m.echo();

  // The AFK channel counts as leaving.
  setVoice(guild, m, 'VA');
  await applyMember(guild, m, settings);
  m.echo();
  setVoice(guild, m, 'AFK');
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rC']);
});

test('a role in both a link and the any-list stays while the member is in voice', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS, voiceRoleAnyRoleIds: ['rA'] });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VB');
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rA', 'rB']);
  m.echo();
  setVoice(guild, m, 'VC');
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rA'], 'rB goes, rA stays (any voice channel)');
});

test('fast hops: A -> B -> A with a lagging role cache still ends with only A', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  // No echo between the hops: member.roles.cache never changes, as when the gateway is slower than the voice moves.
  setVoice(guild, m, 'VA');
  await applyMember(guild, m, settings);
  setVoice(guild, m, 'VB');
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rB']);
  setVoice(guild, m, 'VA');
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rA'], 'the roles from the earlier hops were taken into account');
  assert.equal(m.roles.cache.has('rB'), false, 'the cache really never changed');
  setVoice(guild, m, null);
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), []);
});

test('a leave queued behind a slow role call still takes the role (the overlay clock is read when the step runs) (review2-B)', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  let release;
  const gate = new Promise((r) => { release = r; });
  const add = m.roles.add;
  m.roles.add = async (id, reason) => { await gate; return add(id, reason); }; // a role call stuck behind a busy REST queue
  const realNow = Date.now;
  const t0 = realNow();
  let clock = t0;
  Date.now = () => clock;
  try {
    setVoice(guild, m, 'VA');
    const join = applyMember(guild, m, settings);
    await sleep(5); // the add is in flight
    clock = t0 + 21_000; // the member leaves 21 s after joining, while the add still waits
    setVoice(guild, m, null);
    const leave = applyMember(guild, m, settings);
    clock = t0 + 25_000; // the add goes through 25 s after the join; the gateway echo has not arrived yet
    release();
    await Promise.all([join, leave]);
  } finally {
    Date.now = realNow;
  }
  assert.deepEqual(m.calls.map((c) => `${c.op}:${c.id}`), ['add:rA', 'remove:rA']);
  assert.deepEqual(truthOf(m), [], 'the member does not keep the role after leaving');
});

test('queued changes act on the latest voice state and never repeat a call', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VA');
  const first = applyMember(guild, m, settings);
  setVoice(guild, m, 'VB');
  const second = applyMember(guild, m, settings);
  setVoice(guild, m, 'VA');
  const third = applyMember(guild, m, settings);
  await Promise.all([first, second, third]);
  assert.deepEqual(truthOf(m), ['rA']);
  assert.deepEqual(m.calls.map((c) => `${c.op}:${c.id}`), ['add:rA'], 'one add in total');
});

test('one member\'s changes run one at a time; different members run side by side', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1', { delay: 15 });
  const other = fakeMember(guild, 'U2', { delay: 15 });
  setVoice(guild, m, 'VA');
  setVoice(guild, other, 'VA');
  const p1 = applyMember(guild, m, settings);
  await sleep(5); // the first add is in flight
  setVoice(guild, m, 'VB');
  const p2 = applyMember(guild, m, settings);
  const p3 = applyMember(guild, other, settings);
  await Promise.all([p1, p2, p3]);
  assert.equal(m.maxInflight, 1, 'never two calls at once for the same member');
  assert.deepEqual(truthOf(m), ['rB']);
  assert.deepEqual(truthOf(other), ['rA']);
});

test('roles the bot cannot assign are skipped and the rest still go; bots and deleted roles are ignored', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({
    voiceRoleLinks: [
      { channelId: 'VA', roleId: 'rA' }, { channelId: 'VA', roleId: 'rHIGH' }, { channelId: 'VA', roleId: 'rMANAGED' },
      { channelId: 'VA', roleId: 'rADMIN' }, { channelId: 'VA', roleId: 'rGONE' },
    ],
  });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VA');
  const res = await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rA'], 'only the safe role is given');
  assert.equal(res.skipped, 3, 'above the bot, managed and dangerous are counted; the deleted role is not looked at');
  assert.equal(res.status, 'ok');

  // Roles the member holds that became unsafe can still be taken away (a role above the bot cannot, a dangerous one can).
  const holder = fakeMember(guild, 'U2', { roles: ['rADMIN', 'rHIGH', 'rA'] });
  setVoice(guild, holder, null);
  const out = await applyMember(guild, holder, settings);
  assert.deepEqual(out.removed.sort(), ['rA', 'rADMIN']);
  assert.deepEqual(truthOf(holder), ['rHIGH'], 'a role above the bot is left alone');

  const bot = fakeMember(guild, 'B1', { bot: true });
  setVoice(guild, bot, 'VA');
  assert.equal((await applyMember(guild, bot, settings)).status, 'skipped');
  assert.deepEqual(bot.calls, []);
  assert.equal((await applyMember(guild, null, settings)).status, 'skipped');
});

test('without Manage Roles nothing is called', async () => {
  const guild = fakeGuild({ botPerms: [] });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VA');
  const res = await applyMember(guild, m, SETTINGS({ voiceRoleLinks: LINKS }));
  assert.equal(res.status, 'blocked');
  assert.deepEqual(m.calls, []);
});

test('API errors: a missing member stops, a missing permission skips that role, anything else never throws', async () => {
  const settings = SETTINGS({ voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'VA', roleId: 'rB' }] });

  const guild = fakeGuild();
  const gone = fakeMember(guild, 'U1');
  gone.fail = () => discordErr(10007);
  setVoice(guild, gone, 'VA');
  assert.equal((await applyMember(guild, gone, settings)).status, 'left');
  assert.equal(gone.calls.length, 1, 'stopped at the first call');

  const partial = fakeMember(guild, 'U2');
  partial.fail = (_op, id) => (id === 'rA' ? discordErr(50013) : null);
  setVoice(guild, partial, 'VA');
  const res = await applyMember(guild, partial, settings);
  assert.equal(res.status, 'ok');
  assert.equal(res.skipped, 1);
  assert.deepEqual(truthOf(partial), ['rB'], 'the other role was still given');

  const broken = fakeMember(guild, 'U3');
  broken.fail = () => discordErr(500);
  setVoice(guild, broken, 'VA');
  assert.equal((await applyMember(guild, broken, settings)).status, 'failed');

  const weird = { id: 'U4', user: { id: 'U4' }, roles: null };
  setVoice(guild, weird, 'VA');
  assert.equal((await applyMember(guild, weird, settings)).status, 'failed', 'a broken member object does not reject');
});

test('a failed call is not remembered: the next change tries again', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  let broken = true;
  m.fail = () => (broken ? discordErr(500) : null);
  setVoice(guild, m, 'VA');
  assert.equal((await applyMember(guild, m, settings)).status, 'failed');
  broken = false;
  await applyMember(guild, m, settings);
  assert.deepEqual(truthOf(m), ['rA']);
});

test('without a voice-state cache the event\'s own state is used', async () => {
  const guild = fakeGuild();
  delete guild.voiceStates;
  const m = fakeMember(guild, 'U1');
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  await applyMember(guild, m, settings, { voice: voiceOf({ channelId: 'VB' }, guild) });
  assert.deepEqual(truthOf(m), ['rB']);
  m.echo();
  await applyMember(guild, m, settings, { voice: null });
  assert.deepEqual(truthOf(m), []);
});

// ── Reconcile ─────────────────────────────────────────────────────────────────

test('reconcile: members in voice get their roles, cached holders who are not in voice lose them, bots are ignored', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: [LINKS[0]], voiceRoleAnyRoleIds: ['rAny'] });
  const m1 = fakeMember(guild, 'U1');
  const m2 = fakeMember(guild, 'U2');
  const m3 = fakeMember(guild, 'U3', { roles: ['rA', 'rC'] });
  const m4 = fakeMember(guild, 'B1', { bot: true });
  const m5 = fakeMember(guild, 'U5', { roles: ['rC'] });
  setVoice(guild, m1, 'VA');
  setVoice(guild, m2, 'VC');
  setVoice(guild, m4, 'VA');
  const res = await reconcileGuild(guild, settings);
  assert.deepEqual({ ...res }, { busy: false, checked: 3, changed: 3, added: 3, removed: 1, failed: 0, truncated: false });
  assert.deepEqual(truthOf(m1), ['rA', 'rAny']);
  assert.deepEqual(truthOf(m2), ['rAny']);
  assert.deepEqual(truthOf(m3), ['rC'], 'the unmanaged role stays');
  assert.deepEqual(m4.calls, []);
  assert.deepEqual(m5.calls, []);
  assert.equal(isReconciling(guild.id), false);

  // A second pass finds nothing to do.
  [m1, m2, m3].forEach((m) => m.echo());
  const again = await reconcileGuild(guild, settings);
  assert.equal(again.changed, 0);
});

test('reconcile is bounded and one pass per guild runs at a time', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: [LINKS[0]] });
  const members = ['U1', 'U2', 'U3'].map((id) => fakeMember(guild, id, { delay: 10 }));
  members.forEach((m) => setVoice(guild, m, 'VA'));

  const capped = await reconcileGuild(guild, settings, { max: 2 });
  assert.equal(capped.checked, 2);
  assert.equal(capped.truncated, true);
  assert.equal(members.filter((m) => m.truth.size).length, 2);
  resetState();

  const first = reconcileGuild(guild, settings);
  assert.equal(isReconciling(guild.id), true);
  const second = await reconcileGuild(guild, settings);
  assert.equal(second.busy, true);
  assert.equal(second.checked, 0);
  const done = await first;
  assert.equal(done.busy, false);
  assert.equal(isReconciling(guild.id), false);
  assert.equal(VOICE_ROLES.RECONCILE_MAX > 0, true);

  // Members without a failure path: a failing call counts as failed and the pass still finishes.
  const guild2 = fakeGuild({ botPerms: [] });
  const m = fakeMember(guild2, 'U1');
  setVoice(guild2, m, 'VA');
  const blocked = await reconcileGuild(guild2, settings);
  assert.equal(blocked.failed, 1);
  assert.equal(blocked.changed, 0);
});

// ── Cleanup hooks and patches ─────────────────────────────────────────────────

test('patches: dropping a channel or a role changes only what it must, and nothing when it is absent', () => {
  const cur = {
    voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'VB', roleId: 'rA' }, { channelId: 'CAT', roleId: 'rCat' }],
    voiceRoleAnyRoleIds: ['rAny', 'rA'],
  };
  assert.deepEqual(patchDropChannel(cur, 'VA'), { voiceRoleLinks: [cur.voiceRoleLinks[1], cur.voiceRoleLinks[2]] });
  assert.deepEqual(patchDropChannel(cur, 'CAT'), { voiceRoleLinks: [cur.voiceRoleLinks[0], cur.voiceRoleLinks[1]] });
  assert.equal(patchDropChannel(cur, 'NOPE'), null);
  assert.equal(patchDropChannel({}, 'VA'), null);
  assert.deepEqual(patchDropRole(cur, 'rA'), { voiceRoleLinks: [cur.voiceRoleLinks[2]], voiceRoleAnyRoleIds: ['rAny'] });
  assert.deepEqual(patchDropRole(cur, 'rAny'), { voiceRoleAnyRoleIds: ['rA'] });
  assert.equal(patchDropRole(cur, 'rNOPE'), null);
  assert.equal(patchDropRole(null, 'rA'), null);
});

test('channelDelete and roleDelete hooks drop the links and ids, even while the module is off', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, {
    voiceRolesEnabled: false,
    voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'VB', roleId: 'rB' }, { channelId: 'CAT', roleId: 'rCat' }],
    voiceRoleAnyRoleIds: ['rAny', 'rB'],
  });
  await onChannelDelete({ guildId: guild.id, channel: { id: 'VA' } });
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks.map((l) => l.channelId), ['VB', 'CAT']);
  await onChannelDelete({ guildId: guild.id, channel: { id: 'CAT' } });
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks.map((l) => l.channelId), ['VB']);
  await onRoleDelete({ guildId: guild.id, role: { id: 'rB' } });
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks, []);
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleAnyRoleIds, ['rAny']);
  const before = structuredClone(settingsRows.get(guild.id));
  await onRoleDelete({ guildId: guild.id, role: { id: 'rZZ' } });
  await onChannelDelete({ guildId: guild.id, channel: { id: 'ZZ' } });
  assert.deepEqual(settingsRows.get(guild.id), before, 'nothing to drop: nothing written');
  await onChannelDelete({ guildId: guild.id, channel: null });
  await onRoleDelete({ guildId: guild.id, role: null });
});

test('channelDelete takes a dropped link\'s role from holders no longer entitled to it, only while the module is on (review2-B)', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, {
    voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'CAT', roleId: 'rCat' }, { channelId: 'VB', roleId: 'rCat' }],
  });
  const inVA = fakeMember(guild, 'U1', { roles: ['rA', 'rCat'] });       // in a channel of the deleted category
  const away = fakeMember(guild, 'U2', { roles: ['rCat'] });             // not in voice
  const inVB = fakeMember(guild, 'U3', { roles: ['rCat'] });             // still entitled by the VB link
  const other = fakeMember(guild, 'U4', { roles: ['rB'] });              // holds no dropped role
  setVoice(guild, inVA, 'VA');
  setVoice(guild, inVB, 'VB');

  await onChannelDelete({ guild, guildId: guild.id, channel: { id: 'CAT' } });
  await vr.__releasesIdle();
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks.map((l) => l.channelId), ['VA', 'VB']);
  assert.deepEqual(truthOf(inVA), ['rA'], 'loses the category role, keeps the channel role');
  assert.deepEqual(truthOf(away), []);
  assert.deepEqual(truthOf(inVB), ['rCat'], 'another link still gives it');
  assert.deepEqual(other.calls, [], 'a member without a dropped role is not touched');
  assert.ok(inVA.calls.every((c) => c.reason === VOICE_ROLES.REMOVE_REASON));

  // A deleted voice channel: its members lose its role even while their cached voice state still shows it.
  inVA.echo();
  await onChannelDelete({ guild, guildId: guild.id, channel: { id: 'VA' } });
  await vr.__releasesIdle();
  assert.deepEqual(truthOf(inVA), []);

  // Module off: the link is still dropped (cleanup), but no role is touched.
  const g2 = fakeGuild();
  await seedSettings(g2.id, { voiceRolesEnabled: false, voiceRoleLinks: [{ channelId: 'CAT', roleId: 'rCat' }] });
  const holder = fakeMember(g2, 'U9', { roles: ['rCat'] });
  await onChannelDelete({ guild: g2, guildId: g2.id, channel: { id: 'CAT' } });
  await vr.__releasesIdle();
  assert.deepEqual(settingsRows.get(g2.id).voiceRoleLinks, []);
  assert.deepEqual(holder.calls, []);
});

// ── Hook filters and hooks ────────────────────────────────────────────────────

test('voiceFilter: joins, leaves and moves only; skips bots, module off, nothing set up; runs when the memo is empty', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, { voiceRoleLinks: LINKS });
  await getSettings(guild.id); // fills the memo
  const ctx = (over = {}) => ({ guildId: guild.id, change: 'join', member: { user: { bot: false } }, ...over });
  assert.equal(voiceFilter(ctx()), true);
  assert.equal(voiceFilter(ctx({ change: 'move' })), true);
  assert.equal(voiceFilter(ctx({ change: 'leave' })), true);
  assert.equal(voiceFilter(ctx({ change: 'state' })), false, 'mute, deafen and stream changes are ignored');
  assert.equal(voiceFilter(ctx({ member: { user: { bot: true } } })), false);

  await seedSettings(guild.id, { voiceRoleLinks: LINKS, voiceRolesEnabled: false });
  await getSettings(guild.id);
  assert.equal(voiceFilter(ctx()), false, 'module off');
  await seedSettings(guild.id, { voiceRoleLinks: [], voiceRoleAnyRoleIds: [] });
  await getSettings(guild.id);
  assert.equal(voiceFilter(ctx()), false, 'nothing set up');
  await seedSettings(guild.id, { voiceRoleAnyRoleIds: ['rAny'] });
  await getSettings(guild.id);
  assert.equal(voiceFilter(ctx()), true, 'an any-role alone is a setup');

  await invalidateSettings(guild.id);
  assert.equal(voiceFilter(ctx()), true, 'no memo: the hook runs and decides');
});

test('cleanup filters skip events that cannot matter and fail open without a memo', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, { voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }], voiceRoleAnyRoleIds: ['rAny'] });
  await getSettings(guild.id);
  const c = (id) => ({ guildId: guild.id, channel: { id } });
  const r = (id) => ({ guildId: guild.id, role: { id } });
  assert.equal(channelDeleteFilter(c('VA')), true);
  assert.equal(channelDeleteFilter(c('VB')), false);
  assert.equal(channelDeleteFilter({ guildId: guild.id, channel: null }), false);
  assert.equal(roleDeleteFilter(r('rA')), true);
  assert.equal(roleDeleteFilter(r('rAny')), true);
  assert.equal(roleDeleteFilter(r('rB')), false);
  assert.equal(roleDeleteFilter({ guildId: guild.id, role: null }), false);
  await invalidateSettings(guild.id);
  assert.equal(channelDeleteFilter(c('VB')), true);
  assert.equal(roleDeleteFilter(r('rB')), true);
});

test('onVoiceStateUpdate applies the member\'s roles from the hook context', async () => {
  const guild = fakeGuild();
  const settings = SETTINGS({ voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VB');
  const ctx = { guild, guildId: guild.id, member: m, change: 'join', newState: guild.voiceStates.cache.get('U1'), getSettings: async () => settings };
  await onVoiceStateUpdate(ctx);
  assert.deepEqual(truthOf(m), ['rB']);
  await onVoiceStateUpdate({ ...ctx, getSettings: async () => null });
  await onVoiceStateUpdate({ ...ctx, getSettings: async () => SETTINGS() });
  assert.equal(m.calls.length, 1, 'no settings or nothing set up: nothing happens');
});

test('onGuildReady reconciles detached; nothing set up means nothing runs', async () => {
  const guild = fakeGuild();
  const settings = await seedSettings(guild.id, { voiceRoleLinks: LINKS });
  const inVoice = fakeMember(guild, 'U1');
  const stale = fakeMember(guild, 'U2', { roles: ['rA'] });
  setVoice(guild, inVoice, 'VA');
  await onGuildReady({ guild, guildId: guild.id, reason: 'startup', getSettings: async () => settings });
  await onGuildReady({ guild, guildId: guild.id, reason: 'startup', getSettings: async () => settings }); // queued once
  await __startupSyncsIdle();
  assert.deepEqual(truthOf(inVoice), ['rA']);
  assert.deepEqual(truthOf(stale), []);

  // The module is turned off while the sync waits in the queue: roles stay where they are.
  const late = fakeGuild();
  const lateSettings = await seedSettings(late.id, { voiceRoleLinks: LINKS });
  const lateMember = fakeMember(late, 'U7');
  setVoice(late, lateMember, 'VA');
  await onGuildReady({ guild: late, guildId: late.id, getSettings: async () => lateSettings });
  await seedSettings(late.id, { voiceRoleLinks: LINKS, voiceRolesEnabled: false });
  await __startupSyncsIdle();
  assert.deepEqual(lateMember.calls, []);

  const idle = fakeGuild();
  const other = fakeMember(idle, 'U9', { roles: ['rA'] });
  await onGuildReady({ guild: idle, guildId: idle.id, getSettings: async () => SETTINGS() });
  await onGuildReady({ guild: idle, guildId: idle.id, getSettings: async () => null });
  await __startupSyncsIdle();
  assert.deepEqual(other.calls, []);
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration and hook order', async () => {
  assert.equal(plugin.id, 'voice-roles');
  assert.equal(plugin.module, 'voiceroles');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id]), [
    ['voiceStateUpdate', 'voice-roles'], ['guildReady', 'voice-roles'], ['channelDelete', 'voice-roles'], ['roleDelete', 'voice-roles'],
  ]);
  assert.deepEqual(plugin.hooks.filter((h) => h.always).map((h) => h.event).sort(), ['channelDelete', 'roleDelete']);
  assert.ok(plugin.hooks.find((h) => h.event === 'voiceStateUpdate').filter, 'the per-voice-event hook has a sync filter');
  assert.ok(plugin.hooks.find((h) => h.event === 'voiceStateUpdate').timeoutMs >= 30_000);
  assert.deepEqual(plugin.jobs, []);
  assert.deepEqual(plugin.components, []);
  for (const event of ['voiceStateUpdate', 'guildReady', 'channelDelete', 'roleDelete']) {
    assert.ok(HOOK_ORDER[event].includes('voice-roles'), event);
  }
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'voice-roles'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => String(e?.message ?? e).includes('voice-roles')), []);
});

function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}

test('/voicerole payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'voicerole');
  assert.equal(json.default_member_permissions, '32');
  assert.deepEqual(json.contexts, [0]);
  assert.deepEqual(json.options.map((o) => [o.name, o.type]), [['add', 1], ['remove', 1], ['any', 2], ['list', 1], ['sync', 1]]);
  const byName = Object.fromEntries(json.options.map((o) => [o.name, o]));
  for (const name of ['add', 'remove']) {
    assert.deepEqual(byName[name].options.map((o) => [o.name, o.type, !!o.required]), [['channel', 7, true], ['role', 8, true]]);
    assert.deepEqual(byName[name].options[0].channel_types, [ChannelType.GuildVoice, ChannelType.GuildStageVoice, ChannelType.GuildCategory]);
  }
  assert.deepEqual(byName.any.options.map((o) => [o.name, o.type]), [['add', 1], ['remove', 1]]);
  for (const sub of byName.any.options) assert.deepEqual(sub.options.map((o) => [o.name, o.type, !!o.required]), [['role', 8, true]]);
  assert.ok(charCount(json) <= 1000, `payload is ${charCount(json)} chars`);
  assert.equal(cmd.module, 'voiceroles');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(cmd.subcommandPermissions, undefined);
});

// ── /voicerole replies ────────────────────────────────────────────────────────

function fakeInteraction(guild, { group = null, sub, channel = null, role = null, callerPosition = 9, callerId = 'ADMIN' } = {}) {
  const it = {
    guild, guildId: guild.id, user: { id: callerId }, createdTimestamp: Date.now(),
    member: { id: callerId, roles: { highest: { position: callerPosition } } },
    deferred: false, replied: false, ephemeral: undefined, payloads: [],
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getRole: () => role,
      getChannel: () => channel,
    },
    async deferReply() { it.deferred = true; it.ephemeral = false; },
    async editReply(p) { it.payloads.push(p); return {}; },
  };
  return it;
}

/** Everything the last reply says, as one searchable string. */
function said(it) {
  const p = it.payloads.at(-1);
  assert.ok(p, 'the command replied');
  assert.deepEqual(p.allowedMentions, { parse: [] });
  return (p.embeds ?? []).map((e) => {
    const j = e.toJSON();
    return [j.title, j.description, ...(j.fields ?? []).flatMap((f) => [f.name, f.value]), j.footer?.text].filter(Boolean).join('\n');
  }).join('\n');
}

async function run(guild, opts) {
  const it = fakeInteraction(guild, opts);
  await cmd.execute(it);
  return said(it);
}

test('/voicerole add: links the channel, warns that the bot takes the role away, refuses unsafe roles', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id);
  const add = (channel, role, extra = {}) => run(guild, { sub: 'add', channel: { id: channel }, role: { id: role }, ...extra });

  const ok = await add('VA', 'rA');
  assert.match(ok, /Voice Role Added/);
  assert.match(ok, /<#VA>/);
  assert.match(ok, /<@&rA>/);
  assert.match(ok, /takes this role away from anyone who is not in the linked channel/);
  assert.match(ok, /role made only for this/);
  assert.match(ok, /\/voicerole sync/);
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks, [{ channelId: 'VA', roleId: 'rA' }]);

  assert.match(await add('CAT', 'rCat'), /any voice channel in the <#CAT> category/);
  assert.match(await add('ST', 'rC'), /Voice Role Added/);
  assert.equal(settingsRows.get(guild.id).voiceRoleLinks.length, 3);

  assert.match(await add('VA', 'rA'), /already gives <@&rA>/);
  assert.match(await add('TXT', 'rB'), /Invalid Channel/);
  assert.match(await add('VB', guild.id), /@everyone/);
  assert.match(await add('VB', 'rMANAGED'), /managed by an integration/);
  assert.match(await add('VB', 'rHIGH'), /above my highest role/);
  assert.match(await add('VB', 'rMID', { callerPosition: 5 }), /above your highest role/);
  assert.match(await add('VB', 'rMID', { callerPosition: 5, callerId: 'OWNER' }), /Voice Role Added/, 'the server owner may use any role below the bot');
  assert.match(await add('VB', 'rADMIN'), /dangerous permissions: Administrator/);
  assert.equal(settingsRows.get(guild.id).voiceRoleLinks.length, 4, 'refused roles were not saved');

  const noPerm = fakeGuild({ botPerms: [] });
  await seedSettings(noPerm.id);
  assert.match(await run(noPerm, { sub: 'add', channel: { id: 'VA' }, role: { id: 'rA' } }), /Manage Roles/);
  assert.deepEqual(settingsRows.get(noPerm.id).voiceRoleLinks, []);
});

test('/voicerole add: the cap, the module-off note', async () => {
  const guild = fakeGuild();
  const full = Array.from({ length: LIMITS.VOICE_ROLE_LINKS }, (_, i) => ({ channelId: `X${i}`, roleId: 'rB' }));
  await seedSettings(guild.id, { voiceRoleLinks: full });
  const text = await run(guild, { sub: 'add', channel: { id: 'VA' }, role: { id: 'rA' } });
  assert.match(text, /List Full/);
  assert.equal(settingsRows.get(guild.id).voiceRoleLinks.length, LIMITS.VOICE_ROLE_LINKS);

  await seedSettings(guild.id, { voiceRolesEnabled: false });
  assert.match(await run(guild, { sub: 'add', channel: { id: 'VA' }, role: { id: 'rA' } }), /\/module enable voiceroles/);
  await seedSettings(guild.id, { voiceRolesEnabled: true });
  assert.doesNotMatch(await run(guild, { sub: 'add', channel: { id: 'VB' }, role: { id: 'rB' } }), /\/module enable voiceroles/);
});

test('/voicerole remove: unlinks and says who keeps the role', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, {
    voiceRoleLinks: [{ channelId: 'VA', roleId: 'rA' }, { channelId: 'VB', roleId: 'rA' }, { channelId: 'VC', roleId: 'rC' }],
  });
  const remove = (channel, role) => run(guild, { sub: 'remove', channel: { id: channel }, role: { id: role } });
  const still = await remove('VA', 'rA');
  assert.match(still, /Voice Role Removed/);
  assert.match(still, /still given somewhere else/);
  const last = await remove('VC', 'rC');
  assert.match(last, /members who have it keep it until you take it from them/);
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleLinks, [{ channelId: 'VB', roleId: 'rA' }]);
  assert.match(await remove('VC', 'rC'), /does not give <@&rC>/);
  assert.match(await remove('TXT', 'rC'), /Invalid Channel/);
});

test('/voicerole any add and remove', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id);
  const any = (sub, role, extra = {}) => run(guild, { group: 'any', sub, role: { id: role }, ...extra });
  const ok = await any('add', 'rAny');
  assert.match(ok, /Voice Role Added/);
  assert.match(ok, /takes this role away from anyone who is not in a voice channel/);
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleAnyRoleIds, ['rAny']);
  assert.match(await any('add', 'rAny'), /already given in every voice channel/);
  assert.match(await any('add', guild.id), /@everyone/);
  assert.match(await any('add', 'rHIGH'), /above my highest role/);
  assert.match(await any('add', 'rADMIN'), /dangerous permissions/);
  assert.match(await any('add', 'rMID', { callerPosition: 5 }), /above your highest role/);

  await seedSettings(guild.id, { voiceRoleAnyRoleIds: ['r1', 'r2', 'r3', 'r4', 'r5'] });
  assert.equal(LIMITS.VOICE_ROLE_ANY, 5);
  assert.match(await any('add', 'rA'), /List Full/);

  await seedSettings(guild.id, { voiceRoleAnyRoleIds: ['rAny'], voiceRoleLinks: [{ channelId: 'VA', roleId: 'rAny' }] });
  assert.match(await any('remove', 'rAny'), /still given somewhere else/);
  assert.deepEqual(settingsRows.get(guild.id).voiceRoleAnyRoleIds, []);
  assert.match(await any('remove', 'rAny'), /is not given in every voice channel/);
});

test('/voicerole list: status, links, problems', async () => {
  const guild = fakeGuild({ botPerms: [] });
  await seedSettings(guild.id, {
    voiceRolesEnabled: false,
    voiceRoleLinks: [
      { channelId: 'VA', roleId: 'rA' }, { channelId: 'CAT', roleId: 'rCat' }, { channelId: 'GONE', roleId: 'rB' },
      { channelId: 'VB', roleId: 'rHIGH' },
    ],
    voiceRoleAnyRoleIds: ['rAny', 'rDELETED'],
  });
  const text = await run(guild, { sub: 'list' });
  assert.match(text, /\/module enable voiceroles/);
  assert.match(text, /Channel roles \(4\/50\)/);
  assert.match(text, /<#VA> → <@&rA>/);
  assert.match(text, /📁 <#CAT> → <@&rCat>/);
  assert.match(text, /the channel was deleted/);
  assert.match(text, /I need the Manage Roles permission/);
  assert.match(text, /Any voice channel \(2\/5\)/);
  assert.match(text, /the role was deleted/);
  assert.match(text, /Gets no roles/);
  assert.match(text, /I need \*\*Manage Roles\*\*/);

  const empty = fakeGuild();
  await seedSettings(empty.id, { voiceRoleIgnoreAfk: false });
  const none = await run(empty, { sub: 'list' });
  assert.match(none, /✅ On/);
  assert.match(none, /\/voicerole add/);
  assert.match(none, /\/voicerole any add/);
  assert.match(none, /Gets the roles too/);

  const many = fakeGuild();
  await seedSettings(many.id, { voiceRoleLinks: Array.from({ length: 50 }, (_, i) => ({ channelId: `VA${i}`, roleId: 'rA' })) });
  const it = fakeInteraction(many, { sub: 'list' });
  await cmd.execute(it);
  const fields = it.payloads.at(-1).embeds[0].toJSON().fields;
  assert.ok(fields.every((f) => f.value.length <= 1024), 'long lists are split into fields that fit');
  assert.ok(fields.some((f) => /continued/.test(f.name)));
});

test('/voicerole sync: refuses while the module is off, then reconciles and reports', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id, { voiceRolesEnabled: false, voiceRoleLinks: LINKS });
  const m = fakeMember(guild, 'U1');
  setVoice(guild, m, 'VA');
  assert.match(await run(guild, { sub: 'sync' }), /Module Is Off[\s\S]*\/module enable voiceroles/);
  assert.deepEqual(m.calls, [], 'nothing was changed');

  await seedSettings(guild.id, { voiceRoleLinks: [] });
  assert.match(await run(guild, { sub: 'sync' }), /Nothing To Sync/);

  await seedSettings(guild.id, { voiceRoleLinks: LINKS });
  const stale = fakeMember(guild, 'U2', { roles: ['rB'] });
  const text = await run(guild, { sub: 'sync' });
  assert.match(text, /Voice Roles Synced/);
  assert.match(text, /Checked \*\*2\*\* members: gave \*\*1\*\* role and took \*\*1\*\* away/);
  assert.deepEqual(truthOf(m), ['rA']);
  assert.deepEqual(truthOf(stale), []);

  const noPerm = fakeGuild({ botPerms: [] });
  await seedSettings(noPerm.id, { voiceRoleLinks: LINKS });
  assert.match(await run(noPerm, { sub: 'sync' }), /Missing Permissions[\s\S]*Manage Roles/);

  // A sync that is already running says so instead of starting a second one.
  const busy = fakeGuild();
  await seedSettings(busy.id, { voiceRoleLinks: LINKS });
  const slow = fakeMember(busy, 'U3', { delay: 20 });
  setVoice(busy, slow, 'VA');
  const first = reconcileGuild(busy, SETTINGS({ voiceRoleLinks: LINKS }));
  assert.match(await run(busy, { sub: 'sync' }), /Already Running/);
  await first;
});

test('/voicerole answers when the settings cannot be read', async () => {
  const guild = fakeGuild();
  const real = prisma.guildSettings;
  prisma.guildSettings = { findUnique: async () => { throw new Error('down'); }, upsert: real.upsert, update: real.update };
  settingsRows.delete(guild.id);
  await invalidateSettings(guild.id);
  try {
    assert.match(await run(guild, { sub: 'list' }), /Settings Unavailable/);
    assert.match(await run(guild, { sub: 'sync' }), /Settings Unavailable/);
  } finally {
    prisma.guildSettings = real;
  }
});

test('/voicerole add answers "Could Not Save" when the write fails', async () => {
  const guild = fakeGuild();
  await seedSettings(guild.id);
  const real = prisma.$transaction;
  prisma.$transaction = async () => { throw new Error('db down'); };
  try {
    assert.match(await run(guild, { sub: 'add', channel: { id: 'VA' }, role: { id: 'rA' } }), /Could Not Save/);
    assert.match(await run(guild, { group: 'any', sub: 'add', role: { id: 'rAny' } }), /Could Not Save/);
    assert.match(await run(guild, { sub: 'remove', channel: { id: 'VA' }, role: { id: 'rA' } }), /Could Not Save/);
    assert.match(await run(guild, { group: 'any', sub: 'remove', role: { id: 'rAny' } }), /Could Not Save/);
  } finally {
    prisma.$transaction = real;
  }
});
