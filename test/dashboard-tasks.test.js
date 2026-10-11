// Dashboard tasks: the bot-side runner of `dashboard.task` jobs (src/services/dashboardTasks/) and the dashboard-side
// runner that queues them and waits (src/dashboard/tasks.js). Offline: the database and Redis are test doubles.
import { test, describe, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
process.env.DATABASE_URL  ??= 'postgresql://test:test@127.0.0.1:1/test';
process.env.REDIS_URL     ??= 'redis://127.0.0.1:1';

const shared = await import('../src/services/dashboardTasks/shared.js');
const { TASKS, executeTask } = await import('../src/services/dashboardTasks/index.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');
const { createTaskRunner } = await import('../src/dashboard/tasks.js');
const { JOB_TYPES, INTERACTIVE_JOB_TYPES } = await import('../src/config/jobTypes.js');
const { plugin } = await import('../src/plugins/dashboard-tasks.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// The task modules import the bot's services, which open the Redis client; close it so the run can end.
after(async () => {
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const GUILD = '900000000000000001';
const ACTOR = '900000000000000002';
const fakeGuild = { id: GUILD, members: { fetch: async (id) => (id === ACTOR ? { id } : Promise.reject(new Error('unknown member'))) } };
const job = (kind, payload = {}) => ({ id: 'job1', guildId: GUILD, payload: { kind, actorId: ACTOR, payload } });

describe('bot side', () => {
  test('the job type and plugin are registered', () => {
    assert.deepEqual(JOB_TYPES['dashboard.task'],
      { owner: 'dashboard-tasks', recurring: false, dedupe: 'dashboard.task:{taskId}', interactive: true });
    assert.deepEqual([...INTERACTIVE_JOB_TYPES], ['dashboard.task'], 'claimed by their own fast loop');
    assert.equal(plugin.id, 'dashboard-tasks');
    assert.equal(plugin.module, null);
    assert.deepEqual(plugin.jobs.map((j) => j.type), [shared.TASK_JOB_TYPE]);
  });

  test('every task kind is well formed and has a run function', () => {
    for (const [kind, task] of Object.entries(TASKS)) {
      assert.match(kind, shared.TASK_KIND_RE, kind);
      assert.equal(typeof task.run, 'function', kind);
    }
  });

  test('a task gets the guild, the actor and its payload; its message is kept short', async () => {
    let seen;
    const tasks = { 'test.echo': { run: async (ctx) => { seen = ctx; return { message: 'x'.repeat(900), data: { id: 7 } }; } } };
    await executeTask(tasks, job('test.echo', { a: 1 }), { client: {}, guild: fakeGuild });
    assert.equal(seen.guild, fakeGuild);
    assert.deepEqual(seen.payload, { a: 1 });
    assert.equal(seen.actor.id, ACTOR);
    assert.deepEqual(seen.actor.member, { id: ACTOR });
    assert.equal(seen.actor.isBotOwner, false);
    assert.equal(seen.taskId, 'job1');
  });

  test('an actor who left the server reaches the task as member null', async () => {
    let seen;
    const tasks = { 'test.echo': { run: async (ctx) => { seen = ctx; return { message: 'ok' }; } } };
    const j = job('test.echo');
    j.payload.actorId = '900000000000000099';
    await executeTask(tasks, j, { client: {}, guild: fakeGuild });
    assert.equal(seen.actor.member, null);
  });

  test('failures are permanent: a TaskError keeps its message, anything else is generic, unknown kinds say so', async () => {
    const tasks = {
      'test.refuse': { run: async () => { throw new shared.TaskError('That member is above the bot.'); } },
      'test.crash': { run: async () => { throw new Error('secret internals'); } },
    };
    await assert.rejects(executeTask(tasks, job('test.refuse'), { client: {}, guild: fakeGuild }),
      (err) => err instanceof PermanentJobError && err.message === 'That member is above the bot.');
    await assert.rejects(executeTask(tasks, job('test.crash'), { client: {}, guild: fakeGuild }),
      (err) => err instanceof PermanentJobError && !err.message.includes('secret'));
    await assert.rejects(executeTask(tasks, job('test.nope'), { client: {}, guild: fakeGuild }),
      (err) => err instanceof PermanentJobError && /cannot do "test.nope"/.test(err.message));
  });
});

describe('dashboard side', () => {
  /** A fake ScheduledJob table and Redis, driven by `script(row)` after each poll. */
  function fakes(script) {
    const rows = new Map();
    const kv = new Map();
    let polls = 0;
    const db = {
      scheduledJob: {
        async findUnique({ where }) {
          polls++;
          const row = rows.get(where.id);
          if (row) script(row, { rows, kv, polls });
          const now = rows.get(where.id);
          return now ? { status: now.status, lastError: now.lastError ?? null } : null;
        },
        async deleteMany({ where }) {
          const row = rows.get(where.id);
          if (row && row.status === where.status) {
            rows.delete(where.id);
            return { count: 1 };
          }
          return { count: 0 };
        },
      },
    };
    const scheduled = [];
    const schedule = async (opts) => {
      scheduled.push(opts);
      rows.set('j1', { status: 'PENDING', ...opts });
      return { id: 'j1', runAt: opts.runAt };
    };
    const runner = createTaskRunner({
      logger: { warn() {} }, waitMs: 50, pollMs: 1, sleep: async () => {}, db, kv: { get: async (k) => kv.get(k) ?? null }, schedule,
    });
    return { runner, scheduled, rows, kv };
  }

  test('queues one attempt for the guild with the actor, and reports the bot\'s message', async () => {
    const f = fakes((row, { rows, kv }) => {
      rows.delete('j1');
      kv.set(shared.taskResultKey('j1'), { ok: true, message: 'Banned alice (case #4).', data: { caseId: 4 } });
    });
    const r = await f.runner.run(GUILD, 'mod.action', { action: 'BAN' }, { actorId: ACTOR });
    assert.deepEqual(r, { status: 'done', message: 'Banned alice (case #4).', data: { caseId: 4 } });
    const q = f.scheduled[0];
    assert.equal(q.type, 'dashboard.task');
    assert.equal(q.guildId, GUILD);
    assert.equal(q.maxAttempts, 1);
    assert.match(q.dedupeKey, /^dashboard\.task:[0-9a-f-]{36}$/);
    assert.deepEqual(q.payload, { kind: 'mod.action', actorId: ACTOR, payload: { action: 'BAN' } });
  });

  test('a finished task without a stored message (Redis down) still reads as done', async () => {
    const f = fakes((row, { rows }) => rows.delete('j1'));
    assert.deepEqual(await f.runner.run(GUILD, 'xp.adjust', {}, { actorId: ACTOR }), { status: 'done', message: 'Done.', data: null });
  });

  test('a refusal comes back with the bot\'s reason', async () => {
    const f = fakes((row) => { row.status = 'FAILED'; row.lastError = 'That member is above the bot.'; });
    const r = await f.runner.run(GUILD, 'mod.action', {}, { actorId: ACTOR });
    assert.deepEqual(r, { status: 'failed', message: 'That member is above the bot.', data: null });
  });

  test('a task nobody picks up is cancelled and reported as offline', async () => {
    const f = fakes(() => {});
    const r = await f.runner.run(GUILD, 'mod.action', {}, { actorId: ACTOR });
    assert.equal(r.status, 'offline');
    assert.equal(f.rows.size, 0, 'the queued job was removed');
  });

  test('a task still running when the wait ends is left alone and reported as pending', async () => {
    const f = fakes((row) => { row.status = 'RUNNING'; });
    const r = await f.runner.run(GUILD, 'giveaway.start', {}, { actorId: ACTOR });
    assert.equal(r.status, 'pending');
    assert.equal(f.rows.get('j1').status, 'RUNNING');
  });

  test('a malformed kind is refused before anything is queued', async () => {
    const f = fakes(() => {});
    await assert.rejects(f.runner.run(GUILD, 'DROP TABLE', {}, { actorId: ACTOR }), TypeError);
    assert.equal(f.scheduled.length, 0);
  });
});
