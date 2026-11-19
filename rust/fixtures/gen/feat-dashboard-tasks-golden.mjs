// Reference data for feat-dashboard-tasks, produced by the JS dashboard-task modules themselves (no servers needed).
// Writes rust/fixtures/golden/feat-dashboard-tasks/golden.json:
//   kinds, constants and keys of shared.js, a TASK_KIND_RE sweep, the read*Payload sweeps (input -> value or the
//   TaskError text), and the messages of mod.action / xp.adjust over stubbed service results (createModerationTasks
//   and createMemberTasks take test doubles), plus executeTask's refusals over a test-double task map.
//
//   node rust/fixtures/gen/feat-dashboard-tasks-golden.mjs
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = 'redis://127.0.0.1:1';
delete process.env.BOT_OWNER_ID;

const shared = await imp('src', 'services', 'dashboardTasks', 'shared.js');
const { TASKS, executeTask } = await imp('src', 'services', 'dashboardTasks', 'index.js');
const mod = await imp('src', 'services', 'dashboardTasks', 'moderation.js');
const members = await imp('src', 'services', 'dashboardTasks', 'members.js');
const access = await imp('src', 'services', 'dashboardTasks', 'channelAccess.js');
const { XP_MAX } = await imp('src', 'config', 'constants.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const GUILD = '900000000000000001';
const BOB = '900000000000000002';
const ALICE = '900000000000000003';
const ROLE = '900000000000000011';

/** input -> { ok: value } | { error: text } */
function sweep(fn, inputs) {
  return inputs.map((input) => {
    try {
      return { input, ok: fn(input) };
    } catch (err) {
      if (!(err instanceof shared.TaskError)) throw err;
      return { input, error: err.message };
    }
  });
}

const long = (n, c = 'x') => c.repeat(n);
const kindInputs = ['mod.action', 'xp.adjust', 'a.b', 'A.b', 'a.B', 'ab', 'a..b', 'a.b.c', 'mod.a1', `a${long(30, 'b')}.c`, `a${long(31, 'b')}.c`,
  'DROP TABLE', '', 'giveaway.start', 'rr.roleTemplate', 'é.b', 'a.b ', ' a.b'];

const actionInputs = [
  null, [], 'x', {}, { action: 'MUTE', userId: ALICE }, { action: 'KICK', userId: 'alice' }, { action: 'KICK', userId: 900000000000000003 },
  { action: 'KICK', userId: ALICE }, { action: 'WARN', userId: ALICE, reason: ' ' }, { action: 'WARN', userId: ALICE, reason: '  spam  ' },
  { action: 'KICK', userId: ALICE, reason: long(513) }, { action: 'KICK', userId: ALICE, reason: long(512) }, { action: 'KICK', userId: ALICE, reason: 5 },
  { action: 'TIMEOUT', userId: ALICE }, { action: 'TIMEOUT', userId: ALICE, durationMs: 60000 }, { action: 'TIMEOUT', userId: ALICE, durationMs: 1.5 },
  { action: 'BAN', userId: ALICE, durationMs: -1 }, { action: 'BAN', userId: ALICE, durationMs: null }, { action: 'BAN', userId: ALICE, durationMs: 9007199254740992 },
  { action: 'BAN', userId: ALICE, durationMs: 604800000, deleteMessageSeconds: 86400 }, { action: 'BAN', userId: ALICE, deleteMessageSeconds: 8 * 86400 },
  { action: 'BAN', userId: ALICE, deleteMessageSeconds: null }, { action: 'SOFTBAN', userId: ALICE }, { action: 'SOFTBAN', userId: ALICE, deleteMessageSeconds: 0 },
  { action: 'SOFTBAN', userId: ALICE, deleteMessageSeconds: 86400.5 }, { action: 'KICK', userId: ALICE, proofUrl: 'ftp://x' },
  { action: 'KICK', userId: ALICE, proofUrl: ' https://x.test/p ' }, { action: 'KICK', userId: ALICE, proofUrl: 5 },
  { action: 'KICK', userId: ALICE, proofUrl: `https://x.test/${long(990)}` }, { action: 'UNBAN', userId: ALICE, proofUrl: 'ftp://ignored', durationMs: 5 },
  { action: 'UNMUTE', userId: ALICE, reason: null }, { action: 'WARN', userId: ALICE, reason: '﻿ a  ' },
];

const xpInputs = [
  null, [], { op: 'double', userId: ALICE }, { op: 'give', userId: '12' }, { op: 'give', userId: ALICE }, { op: 'give', userId: ALICE, amount: 0 },
  { op: 'give', userId: ALICE, amount: 1 }, { op: 'give', userId: ALICE, amount: 1000000 }, { op: 'give', userId: ALICE, amount: 1000001 },
  { op: 'give', userId: ALICE, amount: 2.5 }, { op: 'take', userId: ALICE, amount: '5' }, { op: 'set', userId: ALICE, amount: 0 },
  { op: 'set', userId: ALICE, amount: XP_MAX }, { op: 'set', userId: ALICE, amount: XP_MAX + 1 }, { op: 'reset', userId: ALICE },
  { op: 'reset', userId: 900000000000000003 }, { op: 'reset' },
];

const accessInputs = [
  null, {}, { checks: [] }, { checks: 'x' }, { checks: Array.from({ length: 26 }, () => ({ channelId: ALICE })) },
  { checks: [{ channelId: ALICE }] }, { checks: [{ channelId: ALICE, needs: ['ViewChannel'], optional: true }] },
  { checks: [{ channelId: ALICE, needs: [] }] }, { checks: [{ channelId: ALICE, needs: ['ManageGuild'] }] },
  { checks: [{ channelId: ALICE, needs: 'ViewChannel' }] }, { checks: [{ channelId: '1' }] }, { checks: [null] },
  { checks: [{ channelId: ALICE, optional: 'yes' }, { channelId: BOB, needs: ['SendMessages', 'CreatePublicThreads'] }] },
];

// ── mod.action messages over stubbed results ───────────────────────────────────

function fakeGuild() {
  const guild = { id: GUILD, ownerId: '900000000000000999' };
  const admin = {
    id: BOB, user: { id: BOB }, guild, permissions: { has: () => true }, roles: { cache: new Map() }, displayName: 'Bob',
  };
  guild.members = { cache: new Map([[BOB, admin], [ALICE, { id: ALICE, displayName: 'Alice' }]]), fetch: async () => null };
  guild.roles = { cache: new Map([[ROLE, { id: ROLE, name: 'Regular' }]]) };
  const client = { users: { cache: new Map(), fetch: async () => null } };
  return { guild, admin, client };
}

const modCases = [
  { payload: { action: 'BAN', userId: ALICE, reason: 'raid', durationMs: 604800000, deleteMessageSeconds: 86400 },
    result: { ok: true, action: 'TEMPBAN', dm: 'sent', warnings: [], escalation: null, infraction: { caseId: 12, type: 'BAN' } } },
  { payload: { action: 'BAN', userId: ALICE }, result: { ok: true, action: 'BAN', dm: 'skipped', warnings: ['w1'], infraction: { caseId: 2, type: 'BAN' } } },
  { payload: { action: 'WARN', userId: ALICE, reason: 'spam' }, count: 2,
    result: { ok: true, action: 'WARN', dm: 'failed', warnings: [], infraction: { caseId: 3, type: 'WARN' },
      escalation: { tierId: 1, action: 'TIMEOUT', ok: true, caseId: 4, error: null } } },
  { payload: { action: 'WARN', userId: ALICE, reason: 'spam' }, count: 5,
    result: { ok: true, action: 'WARN', dm: 'sent', warnings: ['A note.'], infraction: { caseId: 3, type: 'WARN' },
      escalation: { tierId: 2, action: 'TEMPBAN', ok: false, caseId: null, error: long(300, 'e') } } },
  { payload: { action: 'WARN', userId: BOB, reason: 'spam' }, count: 1,
    result: { ok: true, action: 'WARN', dm: 'sent', warnings: [], infraction: { caseId: 9, type: 'WARN' }, escalation: { tierId: 3, action: 'ODD', ok: false } } },
  { payload: { action: 'TIMEOUT', userId: ALICE, durationMs: 3600000 },
    result: { ok: true, action: 'TIMEOUT', dm: 'sent', warnings: [], infraction: { caseId: 5, type: 'TIMEOUT' } } },
  { payload: { action: 'TIMEOUT', userId: ALICE, durationMs: 40 * 86400000 },
    result: { ok: true, action: 'MUTE', dm: 'sent', warnings: [], infraction: { caseId: 6, type: 'MUTE' } } },
  { payload: { action: 'KICK', userId: '900000000000000077' }, result: { ok: true, action: 'KICK', dm: 'sent', warnings: [], infraction: { caseId: 7, type: 'KICK' } } },
  { payload: { action: 'SOFTBAN', userId: ALICE }, result: { ok: true, action: 'SOFTBAN', dm: 'sent', warnings: [], infraction: { caseId: 8, type: 'SOFTBAN' } } },
  { payload: { action: 'SOFTBAN', userId: ALICE, deleteMessageSeconds: 129600 },
    result: { ok: true, action: 'SOFTBAN', dm: 'sent', warnings: [], infraction: { caseId: 8, type: 'BAN' } } },
  { payload: { action: 'UNBAN', userId: ALICE }, result: { ok: true, infraction: { caseId: 10, type: 'UNBAN' }, cleared: 3 } },
  { payload: { action: 'UNBAN', userId: ALICE }, result: { ok: true, infraction: { caseId: 10, type: 'UNBAN' }, cleared: 1 } },
  { payload: { action: 'UNMUTE', userId: ALICE }, result: { ok: true, infraction: { caseId: 11, type: 'UNMUTE' }, removed: ['timeout', 'mute role'], cleared: 1, note: 'A note.' } },
  { payload: { action: 'UNMUTE', userId: ALICE }, result: { ok: true, infraction: { caseId: 11, type: 'UNMUTE' }, removed: [], cleared: 0 } },
  { payload: { action: 'KICK', userId: ALICE }, result: { ok: false, code: 'HIERARCHY', error: 'That member is above you.' } },
  { payload: { action: 'KICK', userId: ALICE }, result: { ok: false, code: 'X' } },
];

async function modMessages() {
  const out = [];
  for (const c of modCases) {
    const { guild, admin, client } = fakeGuild();
    const stub = async () => c.result;
    const tasks = mod.createModerationTasks({
      getSettingsSafe: async () => ({ modEnabled: true, modRoleIds: [] }),
      executeModAction: stub, executeUnban: stub, executeUnmute: stub,
      prisma: { infraction: { count: async () => c.count ?? 0 } },
    });
    const name = guild.members.cache.get(c.payload.userId)?.displayName ?? c.payload.userId;
    try {
      const r = await tasks['mod.action'].run({ guild, client, actor: { id: BOB, member: admin, isBotOwner: false }, payload: c.payload });
      out.push({ ...c, name, activeWarns: c.payload.action === 'WARN' ? (c.count ?? 0) : null, message: r.message, data: r.data });
    } catch (err) {
      if (!(err instanceof shared.TaskError)) throw err;
      out.push({ ...c, name, error: err.message });
    }
  }
  return out;
}

// ── xp.adjust messages over stubbed results ────────────────────────────────────

const xpCases = [
  { payload: { op: 'give', userId: ALICE, amount: 100 }, applyXpDelta: { xp: 250, oldLevel: 1, newLevel: 2, created: false },
    handleLevelChange: { leveledUp: true, roles: { added: [ROLE], removed: [], failed: [] } } },
  { payload: { op: 'give', userId: ALICE, amount: 1000000 }, applyXpDelta: { xp: XP_MAX, oldLevel: 50, newLevel: 50, created: false },
    handleLevelChange: { leveledUp: false } },
  { payload: { op: 'give', userId: ALICE, amount: 12345 }, applyXpDelta: { xp: 1234567, oldLevel: 3, newLevel: 9, created: true },
    handleLevelChange: { leveledUp: true, roles: { added: [ROLE, '900000000000000012'], removed: ['900000000000000013'], failed: [{ roleId: ROLE, reason: 'x' }] } } },
  { payload: { op: 'take', userId: ALICE, amount: 100 }, subtractXp: { xp: 50, oldXp: 150, oldLevel: 1, newLevel: 0 },
    handleLevelChange: { leveledUp: false, roles: { added: [], removed: [ROLE], failed: [] } } },
  { payload: { op: 'take', userId: ALICE, amount: 100 }, subtractXp: { xp: 0, oldXp: 0, oldLevel: 0, newLevel: 0 } },
  { payload: { op: 'take', userId: ALICE, amount: 100 }, subtractXp: null },
  { payload: { op: 'set', userId: ALICE, amount: 500 }, setXp: { xp: 500, oldLevel: 1, newLevel: 3, created: false }, handleLevelChange: { leveledUp: true } },
  { payload: { op: 'set', userId: ALICE, amount: 0 }, setXp: null },
  { payload: { op: 'set', userId: ALICE, amount: 0 }, setXp: { xp: 0, oldLevel: 2, newLevel: 0 }, handleLevelChange: {} },
  { payload: { op: 'reset', userId: ALICE }, resetXp: { oldLevel: 4 }, syncLevelRoles: { added: [], removed: [ROLE], failed: [] } },
  { payload: { op: 'reset', userId: ALICE }, resetXp: { oldLevel: 7 }, syncLevelRoles: { added: [], removed: [], failed: [] } },
  { payload: { op: 'reset', userId: ALICE }, resetXp: null },
];

async function xpMessages() {
  const out = [];
  for (const c of xpCases) {
    const { guild, admin, client } = fakeGuild();
    guild.members.cache.set(ALICE, { id: ALICE, displayName: 'Alice', user: { id: ALICE, bot: false } });
    const pick = (name, fallback) => async () => (Object.hasOwn(c, name) ? c[name] : fallback);
    const tasks = members.createMemberTasks({
      getSettingsSafe: async () => ({ levelRolesSyncOnDemotion: true }),
      applyXpDelta: pick('applyXpDelta'), subtractXp: pick('subtractXp'), setXp: pick('setXp'), resetXp: pick('resetXp'),
      handleLevelChange: pick('handleLevelChange', {}), syncLevelRoles: pick('syncLevelRoles', {}),
    });
    try {
      const r = await tasks['xp.adjust'].run({ guild, client, actor: { id: BOB, member: admin, isBotOwner: false }, payload: c.payload });
      out.push({ ...c, name: 'Alice', message: r.message, data: r.data });
    } catch (err) {
      if (!(err instanceof shared.TaskError)) throw err;
      out.push({ ...c, name: 'Alice', error: err.message });
    }
  }
  return out;
}

// ── executeTask over a test-double map ────────────────────────────────────────

async function executeCases() {
  const guild = { id: GUILD, members: { fetch: async (id) => (id === BOB ? { id } : Promise.reject(new Error('unknown member'))) } };
  const tasks = {
    'test.refuse': { run: async () => { throw new shared.TaskError(`${long(600, 'r')}`); } },
    'test.crash': { run: async () => { throw new Error('secret internals'); } },
  };
  const out = [];
  for (const kind of [undefined, null, 5, 'test.nope', long(60, 'k'), 'test.refuse', 'test.crash', { a: 1 }, ['a', 'b']]) {
    const payload = kind === undefined ? { actorId: BOB } : { kind, actorId: BOB };
    try {
      await executeTask(tasks, { id: 'job1', guildId: GUILD, payload }, { client: {}, guild });
      out.push({ kind: kind === undefined ? '__undefined__' : kind, ok: true });
    } catch (err) {
      out.push({ kind: kind === undefined ? '__undefined__' : kind, error: err.message, name: err.name });
    }
  }
  return out;
}

// ── giveaway and poll texts over stubbed service results ───────────────────────

const { ChannelType, PermissionsBitField } = await imp('node_modules', 'discord.js', 'src', 'index.js');
const gwMod = await imp('src', 'services', 'dashboardTasks', 'giveaways.js');
const pollMod = await imp('src', 'services', 'dashboardTasks', 'polls.js');
const CH = '920000000000000001';
const NOW = new Date('2026-05-02T12:00:00.000Z');
const NAMES = { '800000000000000001': 'Alice', '800000000000000002': 'Bob', '800000000000000003': 'Carol', '800000000000000004': 'Dan' };

function richGuild() {
  const { guild, admin, client } = fakeGuild();
  for (const [id, displayName] of Object.entries(NAMES)) guild.members.cache.set(id, { id, displayName });
  guild.members.me = { id: '600000000000000999' };
  const all = new PermissionsBitField(PermissionsBitField.All);
  const channel = {
    id: CH, name: 'general', type: ChannelType.GuildText, guild, isTextBased: () => true, isThread: () => false,
    permissionsFor: () => all, toString: () => `<#${CH}>`,
  };
  guild.channels = { cache: new Map([[CH, channel]]), fetch: async () => channel };
  return { guild, admin, client, channel };
}

async function runTask(tasks, kind, payload, ctx) {
  try {
    const r = await tasks[kind].run({ ...ctx, payload });
    return { message: r.message, data: r.data };
  } catch (err) {
    if (!(err instanceof shared.TaskError)) throw err;
    return { error: err.message };
  }
}

const GID = 'cm0abcdefghijklmnopqrstu';
const winners = (n) => Array.from({ length: n }, (_, i) => `80000000000000000${i + 1}`);
const endResults = [
  { ok: true, winnerIds: [], announced: true, payWarnings: [] },
  { ok: true, winnerIds: winners(1), announced: true, payWarnings: [] },
  { ok: true, winnerIds: winners(7), announced: false, payWarnings: ['⚠️ **Alice** could not get the XP prize in <#123>.'] },
  { ok: true, winnerIds: winners(2), announced: true, payWarnings: ['Plain warning.', 'second'] },
  { ok: false, code: 'NOT_ACTIVE' }, { ok: false, code: 'NOT_FOUND' }, { ok: false, code: 'DRAFTED' },
  { ok: false, code: 'NO_ACCESS', missing: ['EmbedLinks', 'ReadMessageHistory'] },
  { ok: false, code: 'NO_ACCESS', missing: [], reason: 'I need **View Channel** in <#555>.' }, { ok: false, code: 'NO_ACCESS', missing: [] },
  { ok: false, code: 'RESOLVE_FAILED' }, { ok: false, code: 'DB_ERROR' },
];
const rerollResults = [
  { ok: true, newWinnerIds: winners(1), announced: true, payWarnings: [] },
  { ok: true, newWinnerIds: winners(6), announced: false, payWarnings: ['⚠️   Coins were not paid.'] },
  { ok: false, code: 'NO_ENTRANTS' }, { ok: false, code: 'NOT_ENDED' }, { ok: false, code: 'NOT_FOUND' }, { ok: false, code: 'CONFLICT' },
  { ok: false, code: 'RESOLVE_FAILED' }, { ok: false, code: 'DB_ERROR' },
];
const startResults = [{ ok: false, code: 'LIMIT' }, { ok: false, code: 'SEND_FAILED' }, { ok: false, code: 'DB_ERROR' }, { ok: true, giveaway: { id: GID } }];
const restartResults = [...startResults.slice(0, 3), { ok: false, code: 'NOT_DRAFT' }, { ok: false, code: 'CONFLICT' }, { ok: true }];

async function giveawayTexts() {
  const out = { end: [], reroll: [], start: [], restart: [] };
  const prize = `A prize ${long(90, 'p')}`;
  for (const [key, kind, status, list, svcName] of [
    ['end', 'giveaway.end', 'ACTIVE', endResults, 'endGiveaway'],
    ['reroll', 'giveaway.reroll', 'ENDED', rerollResults, 'rerollGiveaway'],
    ['start', 'giveaway.start', 'ACTIVE', startResults, 'startGiveaway'],
    ['restart', 'giveaway.restart', 'DRAFT', restartResults, 'restartGiveaway'],
  ]) {
    for (const result of list) {
      const { guild, admin, client, channel } = richGuild();
      const row = { id: GID, guildId: GUILD, status, prize, channelId: CH, endsAt: new Date(NOW.getTime() + 7_200_000), roleWeights: [] };
      const tasks = gwMod.createGiveawayTasks({
        service: { [svcName]: async () => result, fetchChannel: async () => channel },
        getSettings: async () => ({ giveawaysEnabled: true, modRoleIds: [], timezone: 'UTC' }),
        db: { giveaway: { findFirst: async () => row } },
        now: () => NOW,
      });
      const payload = kind === 'giveaway.start' ? { prize, channelId: CH, duration: '1h' } : { giveawayId: GID };
      out[key].push({ result, prize, ...(await runTask(tasks, kind, payload, { guild, client, actor: { id: BOB, member: admin, isBotOwner: false } })) });
    }
  }
  return out;
}

const pollStarts = [
  { ok: false, code: 'LIMIT' }, { ok: false, code: 'SEND', error: { code: 50013 } }, { ok: false, code: 'SEND', error: { code: 50083 } },
  { ok: false, code: 'SEND', error: { code: 10003 } }, { ok: false, code: 'SEND', error: { code: 40005 } }, { ok: false, code: 'SEND', error: {} },
  { ok: false, code: 'SAVE' },
  { ok: true, poll: { id: GID, endsAt: new Date('2026-05-03T12:00:00.000Z') }, timerFailed: true },
  { ok: true, poll: { id: GID, endsAt: null }, timerFailed: false },
];
const pollEnds = [{ ok: true, posted: true }, { ok: true, posted: false }, { ok: false, code: 'ALREADY_ENDED' }, { ok: false, code: 'NOT_FOUND' }];

async function pollTexts() {
  const out = { start: [], end: [] };
  const question = `Which ${long(90, 'q')}?`;
  for (const [i, result] of pollStarts.entries()) {
    const { guild, admin, client } = richGuild();
    const tasks = pollMod.createPollTasks({
      service: { postPoll: async () => result, channelProblem: () => null },
      getSettings: async () => ({ pollsEnabled: true, modRoleIds: [] }),
      db: {}, schedule: async () => ({}),
    });
    const payload = { question, options: ['A', 'B', 'C'], channelId: CH, maxChoices: i % 2 ? 5 : 2, duration: result.poll?.endsAt ? '1d' : null };
    out.start.push({ result, question, payload, ...(await runTask(tasks, 'poll.start', payload, { guild, client, actor: { id: BOB, member: admin, isBotOwner: false } })) });
  }
  for (const result of pollEnds) {
    const { guild, admin, client } = richGuild();
    const tasks = pollMod.createPollTasks({
      service: { endPoll: async () => result },
      getSettings: async () => ({ pollsEnabled: true, modRoleIds: [] }),
      db: { poll: { findFirst: async () => ({ id: GID, question, channelId: CH, status: 'ACTIVE' }) } },
    });
    out.end.push({ result, question, ...(await runTask(tasks, 'poll.end', { pollId: GID }, { guild, client, actor: { id: BOB, member: admin, isBotOwner: false } })) });
  }
  return out;
}

const golden = {
  names: NAMES,
  now: NOW.toISOString(),
  giveawayTexts: await giveawayTexts(),
  pollTexts: await pollTexts(),
  kinds: Object.keys(TASKS),
  constants: {
    TASK_JOB_TYPE: shared.TASK_JOB_TYPE,
    TASK_RESULT_TTL_SECONDS: shared.TASK_RESULT_TTL_SECONDS,
    TASK_MESSAGE_MAX: shared.TASK_MESSAGE_MAX,
    resultKey: shared.taskResultKey('cm0abcdef0123456789abcdef'),
    dedupeKey: shared.taskDedupeKey('0b6f1e9e-58c5-4a51-9d55-a8c1a3f7e2b4'),
    XP_STEP_MAX: members.XP_STEP_MAX,
    XP_OPS: members.XP_OPS,
    DASHBOARD_MOD_ACTIONS: mod.DASHBOARD_MOD_ACTIONS,
    COMMAND_REASON_MAX: mod.COMMAND_REASON_MAX,
    ACCESS_NEEDS: access.ACCESS_NEEDS,
    ACCESS_CHECKS_MAX: access.ACCESS_CHECKS_MAX,
  },
  kindSweep: kindInputs.map((k) => ({ kind: k, ok: shared.TASK_KIND_RE.test(k) })),
  actionPayloads: sweep(mod.readActionPayload, actionInputs),
  xpPayloads: sweep(members.readXpPayload, xpInputs),
  accessPayloads: sweep(access.readAccessPayload, accessInputs),
  modMessages: await modMessages(),
  xpMessages: await xpMessages(),
  execute: await executeCases(),
};

const outDir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-dashboard-tasks');
mkdirSync(outDir, { recursive: true });
writeFileSync(join(outDir, 'golden.json'), `${JSON.stringify(golden, null, 2)}\n`);
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.stdout.write(`wrote ${join(outDir, 'golden.json')}\n`);
process.exit(0);
