// Security regression (finding f04): one guild's slow dashboard task must not stall every other guild's dashboard tasks on
// the shard. The interactive scheduler loop used to claim a batch and await all of it, and ticket.refresh back-filled manager
// overwrites (up to 50 sequential REST calls per role) inside the task, so one admin could hold the lane past the dashboard's
// 12 s wait and have other guilds' tasks cancelled as "bot offline". The back-fill now runs as the guild's ONE ordinary
// ticket.managersync job, which a guild cannot multiply, reset while running, or let hold a slot for long.
// Offline: the ScheduledJob table is a test double that follows the claim's rules. With TEST_DATABASE_URL (a migrated
// database) the real claim and merge SQL runs too:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/sec-dashboard-tasks.test.js
import { test, describe, after, mock } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache falls back, and Postgres is only used with TEST_DATABASE_URL.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const scheduler = await import('../src/services/scheduler.js');
const { JOB_TYPES, INTERACTIVE_JOB_TYPES } = await import('../src/config/jobTypes.js');
const { TASK_WAIT_MS } = await import('../src/dashboard/tasks.js');
const { tasks: ticketTasks } = await import('../src/services/dashboardTasks/tickets.js');
const { runManagerSyncJob, managerSyncPayload, mergeManagerSync } = await import('../src/services/ticketService.js');
const { plugin: ticketsPlugin } = await import('../src/plugins/tickets.js');
const { default: prisma } = await import('../src/database/client.js');
const { default: redis } = await import('../src/services/redis.js');

// Silence the Redis reconnect and logger noise (the logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

after(async () => {
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const { fillInteractiveSlots, INTERACTIVE_CONCURRENCY, start, stop, RAW_SQL } = scheduler;
const GUILD_A = '960000000000000001';
const GUILD_B = '960000000000000002';
const guildN = (n) => `9610000000000${String(n).padStart(5, '0')}`;
const jobRow = (id, guildId) => ({
  id, guildId, type: 'dashboard.task', payload: {}, runAt: new Date(), attempts: 1, maxAttempts: 1, dedupeKey: null, version: 0,
});
const clientFor = (guildIds) => ({ guilds: { cache: new Map(guildIds.map((id) => [id, { id, available: true }])) }, shard: null });
const eventually = async (fn, ms = 2000) => {
  const end = Date.now() + ms;
  while (!fn()) {
    if (Date.now() > end) return false;
    await new Promise((resolve) => { setTimeout(resolve, 5); });
  }
  return true;
};
const flush = async () => { for (let i = 0; i < 20; i++) await new Promise((resolve) => { setImmediate(resolve); }); };

// ── The dashboard.task handler used by the loop tests: runs, or waits for a gate the test opens ──────────────────────────

const gates = new Map();
const trace = { started: [], finished: [], activeByGuild: new Map(), peakByGuild: new Map(), active: 0, peak: 0 };
const resetTrace = () => {
  gates.clear();
  Object.assign(trace, { started: [], finished: [], activeByGuild: new Map(), peakByGuild: new Map(), active: 0, peak: 0 });
};
/** Holds job `id` until the returned function is called. */
function gate(id) {
  let release;
  const promise = new Promise((resolve) => { release = resolve; });
  gates.set(id, { promise, release });
  return release;
}
scheduler.registerJobHandler('dashboard.task', async (job) => {
  const now = (trace.activeByGuild.get(job.guildId) ?? 0) + 1;
  trace.activeByGuild.set(job.guildId, now);
  trace.peakByGuild.set(job.guildId, Math.max(trace.peakByGuild.get(job.guildId) ?? 0, now));
  trace.active++;
  trace.peak = Math.max(trace.peak, trace.active);
  trace.started.push(job.id);
  try {
    await (gates.get(job.id)?.promise ?? new Promise((resolve) => { setImmediate(resolve); }));
  } finally {
    trace.activeByGuild.set(job.guildId, trace.activeByGuild.get(job.guildId) - 1);
    trace.active--;
    trace.finished.push(job.id);
  }
});

/**
 * A ScheduledJob table for the interactive claim: it hands out due rows the way claimSql does (at most `limit`, at most one
 * per guild, none for a guild with a row running) and frees a guild when its row is deleted. The SQL itself is checked in the
 * RAW_SQL test and, with a database, in the DB test.
 */
function fakeTable() {
  const queue = [];
  const running = new Map();
  const claims = [];
  const real = {
    queryRaw: prisma.$queryRaw, deleteMany: prisma.scheduledJob.deleteMany, updateMany: prisma.scheduledJob.updateMany,
  };
  prisma.$queryRaw = async (sql) => {
    if (sql.strings.join('?').includes('NOT (type')) return []; // the ordinary loop
    const limit = sql.values.find((v) => typeof v === 'number');
    const busy = new Set(running.values());
    const picked = [];
    for (const row of queue) {
      if (picked.length >= limit) break;
      if (busy.has(row.guildId)) continue;
      busy.add(row.guildId);
      picked.push(row);
    }
    for (const row of picked) {
      queue.splice(queue.indexOf(row), 1);
      running.set(row.id, row.guildId);
    }
    claims.push({ limit, ids: picked.map((r) => r.id) });
    return picked;
  };
  const finish = async ({ where }) => {
    running.delete(where.id);
    return { count: 1 };
  };
  prisma.scheduledJob.deleteMany = finish;
  prisma.scheduledJob.updateMany = finish;
  return {
    queue, running, claims,
    restore() {
      prisma.$queryRaw = real.queryRaw;
      prisma.scheduledJob.deleteMany = real.deleteMany;
      prisma.scheduledJob.updateMany = real.updateMany;
    },
  };
}

/** Runs `body(table)` with the loop started on a fresh table; releases every gate and stops the loop afterwards. */
async function withLoop(body, { fakeTimers = false } = {}) {
  resetTrace();
  const table = fakeTable();
  if (fakeTimers) mock.timers.enable({ apis: ['setTimeout'] });
  try {
    await body(table);
  } finally {
    for (const { release } of gates.values()) release();
    await stop({ timeoutMs: 1000 });
    if (fakeTimers) mock.timers.reset();
    table.restore();
  }
}

// ── The interactive loop ─────────────────────────────────────────────────────────────────────────────────────────────

describe('interactive scheduler loop', () => {
  test('a slow task in guild A does not keep guild B\'s task from being claimed and run', async () => {
    await withLoop(async (table) => {
      const client = clientFor([GUILD_A, GUILD_B]);
      start(client);
      const releaseA = gate('a1');
      table.queue.push(jobRow('a1', GUILD_A));
      assert.equal(await fillInteractiveSlots(client), 1);
      assert.ok(await eventually(() => trace.started.includes('a1')));

      // B asks while A's task is still running: the claim comes back at once and B's task runs.
      table.queue.push(jobRow('b1', GUILD_B));
      assert.equal(await fillInteractiveSlots(client), 1, 'B is claimed while A holds its slot');
      assert.ok(await eventually(() => trace.finished.includes('b1')), 'B ran to the end');
      assert.equal(trace.finished.includes('a1'), false, 'A is still running');

      releaseA();
      assert.ok(await eventually(() => trace.finished.includes('a1')));
    });
  });

  test('the poll loop waits only for the claim: a task queued later is claimed while an earlier one is still running', async () => {
    await withLoop(async (table) => {
      const client = clientFor([GUILD_A, GUILD_B]);
      start(client);
      gate('slow-a');
      table.queue.push(jobRow('slow-a', GUILD_A));
      mock.timers.tick(6_100); // the idle poll (about 5 s, +20 % jitter at most)
      await flush();
      assert.deepEqual(trace.started, ['slow-a']);

      table.queue.push(jobRow('b-1', GUILD_B));
      mock.timers.tick(1_300); // the next poll is the fast one (about 1 s): it must not be waiting for slow-a
      await flush();
      assert.deepEqual(trace.started, ['slow-a', 'b-1']);
      assert.ok(trace.finished.includes('b-1'));
      assert.equal(trace.finished.includes('slow-a'), false);
    }, { fakeTimers: true });
  });

  test('a guild cannot occupy more than one interactive slot; its next task starts when the last one ends', async () => {
    await withLoop(async (table) => {
      const client = clientFor([GUILD_A, GUILD_B]);
      start(client);
      const release = { a1: gate('a1'), a2: gate('a2'), a3: gate('a3') };
      table.queue.push(jobRow('a1', GUILD_A), jobRow('a2', GUILD_A), jobRow('a3', GUILD_A), jobRow('b1', GUILD_B));
      assert.equal(await fillInteractiveSlots(client), 2, 'one job per guild: a1 and b1');
      assert.ok(await eventually(() => trace.finished.includes('b1') && trace.started.includes('a1')));
      assert.deepEqual(trace.started.filter((id) => id.startsWith('a')), ['a1'], 'a2 and a3 wait behind a1');

      release.a1();
      assert.ok(await eventually(() => trace.started.includes('a2')), 'a finished job refills its slot at once');
      assert.equal(trace.started.includes('a3'), false);
      release.a2();
      assert.ok(await eventually(() => trace.started.includes('a3')));
      release.a3();
      assert.ok(await eventually(() => trace.finished.includes('a3')));
      assert.deepEqual(trace.finished.filter((id) => id.startsWith('a')), ['a1', 'a2', 'a3']);
      assert.equal(trace.peakByGuild.get(GUILD_A), 1, 'never two of guild A at once');
    });
  });

  test('it claims only as many jobs as there are free slots, and does not wait for them', async () => {
    await withLoop(async (table) => {
      const guilds = Array.from({ length: INTERACTIVE_CONCURRENCY + 3 }, (_, n) => guildN(n));
      const client = clientFor(guilds);
      start(client);
      const release = guilds.map((_, n) => gate(`j${n}`));
      guilds.forEach((g, n) => table.queue.push(jobRow(`j${n}`, g)));

      // Every handler is held, yet the claim returned: the loop is not waiting for the batch.
      assert.equal(await fillInteractiveSlots(client), INTERACTIVE_CONCURRENCY);
      assert.equal(table.claims[0].limit, INTERACTIVE_CONCURRENCY, 'the claim asked for the free slots, not a batch of 25');
      assert.ok(await eventually(() => trace.started.length === INTERACTIVE_CONCURRENCY));
      assert.equal(table.queue.length, 3, 'the rest stay queued, unclaimed');
      assert.equal(await fillInteractiveSlots(client), 0, 'no free slot, no claim');
      assert.equal(table.claims.length, 1);

      release[0]();
      assert.ok(await eventually(() => trace.started.length === INTERACTIVE_CONCURRENCY + 1), 'the freed slot is refilled');
      assert.equal(table.claims.at(-1).limit, 1);
      assert.ok(trace.peak <= INTERACTIVE_CONCURRENCY, `peak ${trace.peak}`);
    });
  });
});

describe('claim SQL', () => {
  const sqlOf = (name) => RAW_SQL.find((q) => q.name === name).build().sql;

  test('interactive claims skip a guild with a job running and take one job per guild; ordinary claims are unchanged', () => {
    for (const name of ['scheduler.claimInteractive', 'scheduler.claimInteractiveUnsharded']) {
      const sql = sqlOf(name);
      assert.match(sql, /NOT EXISTS \(\s*SELECT 1 FROM "ScheduledJob" r\s+WHERE r\."guildId" = j\."guildId"[\s\S]*?r\.status = 'RUNNING'/, `${name}: a running job holds its guild`);
      assert.match(sql, /r\."updatedAt" > .* - interval '90 seconds'/, `${name}: a crashed process's row stops counting`);
      assert.match(sql, /\(e\."runAt", e\.id\) < \(j\."runAt", j\.id\)/, `${name}: a claim takes a guild's earliest due job only`);
      assert.match(sql, /FOR UPDATE SKIP LOCKED/);
    }
    assert.doesNotMatch(sqlOf('scheduler.claim'), /NOT EXISTS/, 'ordinary jobs are not limited per guild');
  });
});

// ── ticket.refresh stays short and hands the back-fill to the guild's one job ────────────────────────────────────────────

describe('ticket.refresh and the ticket.managersync job', () => {
  const GUILD = '962000000000000001';
  const PANEL = 'panel_f04';
  const role = (n) => `9630000000000${String(n).padStart(5, '0')}`;
  const KEY = `ticket.managersync:${GUILD}`;

  /**
   * A guild with `tickets` ticket channels whose permission edits take `overwriteMs` each, and stubs for the database,
   * including a ScheduledJob table that follows scheduleJob's and settleMergedJob's locking (a plain Map: the rows are
   * read and written under the same lock the SQL takes).
   */
  function world({ tickets = 60, overwriteMs = 20, managers = [role(1), role(2)], extraRoles = [], onFirstCall = null } = {}) {
    const calls = [];
    const reads = [];
    const table = new Map();
    let seq = 0;
    let firstCallDone = false;
    const channels = new Map();
    const pause = () => (overwriteMs ? new Promise((resolve) => { setTimeout(resolve, overwriteMs); }) : Promise.resolve());
    const touch = async (kind, id, roleId) => {
      calls.push([kind, id, roleId]);
      if (onFirstCall && !firstCallDone) {
        firstCallDone = true;
        await onFirstCall();
      }
      await pause();
    };
    for (let i = 0; i < tickets; i++) {
      const id = `964${String(i).padStart(15, '0')}`;
      channels.set(id, {
        id,
        permissionOverwrites: { edit: (roleId) => touch('edit', id, roleId), delete: (roleId) => touch('delete', id, roleId) },
      });
    }
    const guild = {
      id: GUILD, available: true,
      roles: { cache: new Map([...managers, ...extraRoles].map((id) => [id, { id }])) },
      channels: { cache: channels },
    };
    const state = { panel: { id: PANEL, guildId: GUILD, name: 'Support', managerRoleIds: [...managers], options: [] } };
    const byKey = (key) => [...table.values()].find((r) => r.dedupeKey === key);
    const tx = {
      $queryRaw: async (sql) => {
        const row = sql.strings.join('?').includes('"dedupeKey"') ? byKey(sql.values[0]) : table.get(sql.values[0]);
        return row ? [structuredClone(row)] : [];
      },
      scheduledJob: {
        create: async ({ data }) => {
          const row = { id: `job${++seq}`, status: 'PENDING', attempts: 0, version: 0, lockedBy: null, ...structuredClone(data) };
          table.set(row.id, row);
          return structuredClone(row);
        },
        update: async ({ where, data }) => {
          const row = table.get(where.id);
          for (const [k, v] of Object.entries(data)) {
            if (v && typeof v === 'object' && 'increment' in v) row[k] += v.increment;
            else row[k] = k === 'payload' ? structuredClone(v) : v;
          }
          return structuredClone(row);
        },
        delete: async ({ where }) => { table.delete(where.id); },
      },
    };
    const real = {
      settingsFind: prisma.guildSettings.findUnique, settingsUpsert: prisma.guildSettings.upsert,
      panelFind: prisma.ticketPanel.findFirst, count: prisma.ticket.count, many: prisma.ticket.findMany, tx: prisma.$transaction,
    };
    prisma.guildSettings.findUnique = async ({ where }) => ({ guildId: where.guildId });
    prisma.guildSettings.upsert = async ({ where }) => ({ guildId: where.guildId });
    prisma.ticketPanel.findFirst = async ({ where }) => {
      const id = where.id ?? where.OR?.[0]?.id; // findPanel looks a panel up by id or name
      return state.panel && where.guildId === GUILD && id === PANEL ? state.panel : null;
    };
    prisma.ticket.count = async () => { reads.push('count'); return channels.size; };
    prisma.ticket.findMany = async () => {
      reads.push('findMany');
      return [...channels.keys()].slice(0, 50).map((channelId) => ({ id: `t${channelId}`, channelId }));
    };
    prisma.$transaction = async (fn) => fn(tx);
    return {
      guild, state, calls, reads, table,
      /** What the bot's claim does to the row: RUNNING, one more attempt, a lock. */
      claim(row) {
        Object.assign(table.get(row.id), { status: 'RUNNING', attempts: row.attempts + 1, lockedBy: 'live', lockedUntil: new Date(Date.now() + 300_000) });
        const { id, guildId, version, payload } = table.get(row.id);
        return { id, guildId, version, payload: structuredClone(payload) };
      },
      only() {
        assert.equal(table.size, 1, `one job row, not ${table.size}`);
        return [...table.values()][0];
      },
      restore() {
        prisma.guildSettings.findUnique = real.settingsFind;
        prisma.guildSettings.upsert = real.settingsUpsert;
        prisma.ticketPanel.findFirst = real.panelFind;
        prisma.ticket.count = real.count;
        prisma.ticket.findMany = real.many;
        prisma.$transaction = real.tx;
      },
    };
  }

  const owner = { id: '965000000000000001', member: null, isBotOwner: true };
  const refresh = (w, payload) => ticketTasks['ticket.refresh'].run({ guild: w.guild, client: {}, actor: owner, payload: { panelId: PANEL, message: false, ...payload }, taskId: 't' });
  const rolesOf = (row) => [...row.payload.panels[PANEL].roles].sort();
  const edits = (w, roleId) => w.calls.filter((c) => c[0] === 'edit' && c[2] === roleId);
  const deletes = (w, roleId) => w.calls.filter((c) => c[0] === 'delete' && c[2] === roleId);

  test('a 10-for-10 manager swap returns quickly, touches no ticket channel and queues ONE job, not 20', async () => {
    const oldRoles = Array.from({ length: 10 }, (_, n) => role(n + 1));
    const newRoles = Array.from({ length: 10 }, (_, n) => role(n + 11));
    const w = world({ managers: newRoles, extraRoles: oldRoles });
    try {
      const started = Date.now();
      const r = await refresh(w, { managersAdded: newRoles, managersRemoved: oldRoles });
      const took = Date.now() - started;
      // The old task made 20 roles x 50 tickets of sequential calls; a task must stay far under the dashboard's wait or it
      // holds an interactive slot while other servers' requests time out.
      assert.ok(took < TASK_WAIT_MS / 10, `the task took ${took} ms`);
      assert.deepEqual(w.calls, [], 'no overwrite is edited inside the task');
      assert.deepEqual(w.reads, [], 'and the tickets are not even read');
      assert.match(r.message, /background/);

      const row = w.only();
      assert.equal(row.type, 'ticket.managersync');
      assert.equal(row.dedupeKey, KEY, 'keyed by guild: no second job can exist');
      assert.equal(row.guildId, GUILD);
      assert.equal(row.maxAttempts, 3);
      assert.ok(row.runAt instanceof Date);
      assert.deepEqual(rolesOf(row), [...oldRoles, ...newRoles].sort(), 'the removed roles are in the job, so they are still removed');
      assert.deepEqual([row.payload.panels[PANEL].done, row.payload.panels[PANEL].rev], [0, 1]);
      assert.ok(!INTERACTIVE_JOB_TYPES.includes(row.type), 'an ordinary job, not an interactive one');
    } finally {
      w.restore();
    }
  });

  test('queues nothing when no manager role changed', async () => {
    const w = world();
    try {
      const r = await refresh(w, {});
      assert.equal(w.table.size, 0);
      assert.doesNotMatch(r.message, /background/);
    } finally {
      w.restore();
    }
  });

  test('more saves, for other panels too, merge into the same job', async () => {
    const w = world({ managers: [role(1), role(2), role(3)] });
    try {
      await refresh(w, { managersAdded: [role(1)], managersRemoved: [role(9)] });
      await refresh(w, { managersAdded: [role(2), role(3)] });
      const row = w.only();
      assert.deepEqual(rolesOf(row), [role(1), role(2), role(3), role(9)]);
      assert.equal(row.payload.panels[PANEL].rev, 2, 'each merge counts up');
      assert.equal(row.version, 0);
      assert.equal(row.status, 'PENDING');
      // another panel of the guild goes into the same row
      const merged = mergeManagerSync(row.payload, managerSyncPayload('other_panel', [role(5)]));
      assert.deepEqual(Object.keys(merged.panels).sort(), ['other_panel', PANEL]);
    } finally {
      w.restore();
    }
  });

  test('a repeated save while the sync is RUNNING does not reset or restart it; the change is kept for the next run', async () => {
    const w = world({ managers: [role(1), role(2)] });
    try {
      await refresh(w, { managersAdded: [role(1)] });
      const job = w.claim(w.only());
      const running = structuredClone(w.only());
      await refresh(w, { managersAdded: [role(2)] });
      const row = w.only();
      for (const field of ['status', 'version', 'attempts', 'lockedBy', 'lockedUntil', 'runAt', 'dedupeKey']) {
        assert.deepEqual(row[field], running[field], `${field} is untouched`);
      }
      assert.equal(row.status, 'RUNNING');
      assert.deepEqual(rolesOf(row), [role(1), role(2)], 'but the new role is in its payload');
      assert.equal(row.payload.panels[PANEL].rev, 2);
      assert.equal(job.version, row.version, 'the run in flight still owns the row');
    } finally {
      w.restore();
    }
  });

  test('the job adds the managers and removes the removed roles from the newest 50 tickets, then ends', async () => {
    const w = world({ overwriteMs: 0, managers: [role(1)], extraRoles: [role(2)] });
    try {
      await refresh(w, { managersAdded: [role(1)], managersRemoved: [role(2)] });
      await runManagerSyncJob(w.claim(w.only()), { guild: w.guild });
      assert.equal(edits(w, role(1)).length, 50);
      assert.equal(deletes(w, role(2)).length, 50, 'a removed role is still removed');
      assert.equal(w.calls.length, 100);
      assert.equal(w.table.size, 0, 'nothing left: the row is gone');
    } finally {
      w.restore();
    }
  });

  /** Queues `payload` for the guild's job directly, as scheduleJob does for the task (or by hand, for a payload the task would not build). */
  const queue = (payload, extra = {}) => scheduler.scheduleJob({
    guildId: GUILD, type: 'ticket.managersync', runAt: new Date(), dedupeKey: KEY, maxAttempts: 3, payload, ...extra,
  });

  test('a removed role is deleted from every ticket whether or not the guild still has the role', async () => {
    const w = world({ overwriteMs: 0, managers: [role(1)] });
    try {
      await refresh(w, { managersRemoved: [role(3)] }); // role(3) is not a manager and not in the guild's role cache
      await runManagerSyncJob(w.claim(w.only()), { guild: w.guild });
      assert.equal(deletes(w, role(3)).length, 50);
      assert.equal(edits(w, role(3)).length, 0);
    } finally {
      w.restore();
    }
  });

  test('a manager role deleted in Discord is skipped, not added', async () => {
    const w = world({ overwriteMs: 0, managers: [role(1)] });
    try {
      w.guild.roles.cache.delete(role(1));
      await queue(managerSyncPayload(PANEL, [role(1)]));
      await runManagerSyncJob(w.claim(w.only()), { guild: w.guild });
      assert.deepEqual(w.calls, []);
      assert.equal(w.table.size, 0, 'and the job ends');
    } finally {
      w.restore();
    }
  });

  test('a panel deleted meanwhile and a payload with nothing valid in it cost nothing and end the job', async () => {
    const w = world({ overwriteMs: 0, managers: [role(1)] });
    try {
      await queue(managerSyncPayload(PANEL, [role(1)]));
      w.state.panel = null;
      await runManagerSyncJob(w.claim(w.only()), { guild: w.guild });
      assert.deepEqual(w.calls, [], 'the panel is gone');
      assert.equal(w.table.size, 0);

      await queue({ panels: { 'bad id!': { roles: [role(1)] }, [PANEL]: { roles: 'nope' } } });
      await runManagerSyncJob(w.claim(w.only()), { guild: w.guild });
      assert.deepEqual(w.calls, []);
      assert.equal(w.table.size, 0);
    } finally {
      w.restore();
    }
  });

  test('a change saved while the job runs is not lost: the job runs again instead of ending', async () => {
    const midRun = { done: false };
    const w = world({
      overwriteMs: 0, managers: [role(1)], extraRoles: [role(2)],
      onFirstCall: async () => {
        // the admin saves again while the job is in the middle of the first panel's tickets
        w.state.panel.managerRoleIds.push(role(2));
        w.guild.roles.cache.set(role(2), { id: role(2) });
        await refresh(w, { managersAdded: [role(2)] });
        midRun.done = true;
      },
    });
    try {
      await refresh(w, { managersAdded: [role(1)] });
      const first = w.claim(w.only());
      await runManagerSyncJob(first, { guild: w.guild });
      assert.ok(midRun.done);
      const row = w.only();
      assert.equal(row.status, 'PENDING', 'back in the queue for a fresh claim');
      assert.equal(row.version, first.version + 1);
      assert.equal(row.attempts, 0);
      assert.equal(row.lockedBy, null);
      assert.deepEqual(rolesOf(row), [role(1), role(2)]);
      assert.equal(row.payload.panels[PANEL].done, 0, 'a panel merged into meanwhile starts over');

      // the follow-up run covers the new role on every ticket and ends
      await runManagerSyncJob(w.claim(row), { guild: w.guild });
      assert.equal(edits(w, role(2)).length, 50);
      assert.equal(w.table.size, 0);
    } finally {
      w.restore();
    }
  });

  test('a run stops at its time budget and the rest goes to follow-up runs, each ticket handled once', async () => {
    const w = world({ overwriteMs: 20, managers: [role(1)] });
    try {
      await refresh(w, { managersAdded: [role(1)] });
      let runs = 0;
      let sawPartial = false;
      while (w.table.size && runs < 200) {
        const job = w.claim(w.only());
        const before = w.calls.length;
        await runManagerSyncJob(job, { guild: w.guild }, { budgetMs: 60 });
        runs++;
        assert.ok(w.calls.length > before, 'every run makes progress');
        if (w.table.size) {
          const row = w.only();
          assert.equal(row.status, 'PENDING', 'the rest is queued');
          assert.equal(row.payload.panels[PANEL].done, w.calls.length, 'resuming where it stopped');
          sawPartial = true;
        }
      }
      assert.ok(sawPartial && runs > 1, `${runs} runs`);
      assert.equal(w.table.size, 0);
      assert.equal(w.calls.length, 50, 'no ticket was handled twice');
      assert.equal(new Set(w.calls.map((c) => c[1])).size, 50);
    } finally {
      w.restore();
    }
  });

  test('scheduleJob merge: needs a function and a dedupe key; resolveConflict never replaces a live row for it', async () => {
    assert.equal(scheduler.resolveConflict({ status: 'RUNNING', runAt: new Date() }, { runAt: new Date() }, 'merge'), 'merge');
    assert.equal(scheduler.resolveConflict({ status: 'PENDING', runAt: new Date() }, { runAt: new Date() }, 'merge'), 'merge');
    assert.equal(scheduler.resolveConflict({ status: 'FAILED', runAt: new Date() }, { runAt: new Date() }, 'merge'), 'replace');
    assert.equal(scheduler.resolveConflict(null, { runAt: new Date() }, 'merge'), 'insert');
    const base = { guildId: GUILD, type: 'ticket.managersync', runAt: new Date() };
    await assert.rejects(scheduler.scheduleJob({ ...base, dedupeKey: KEY, onConflict: 'merge' }), /merge function/);
    await assert.rejects(scheduler.scheduleJob({ ...base, onConflict: 'merge', merge: () => ({}) }), /dedupeKey/);
  });

  test('the job type is an ordinary one owned by the tickets plugin', () => {
    assert.deepEqual(JOB_TYPES['ticket.managersync'], { owner: 'tickets', recurring: false, dedupe: 'ticket.managersync:{guildId}' });
    assert.equal(INTERACTIVE_JOB_TYPES.includes('ticket.managersync'), false);
    assert.deepEqual(ticketsPlugin.jobs.map((j) => [j.type, j.handler]), [['ticket.managersync', runManagerSyncJob]]);
  });
});

// ── The real SQL ─────────────────────────────────────────────────────────────────────────────────────────────────────────

describe('SQL against Postgres', () => {
  const ID = (n) => `96400000000${String(n).padStart(5, '0')}`;
  const inSeconds = (s) => new Date(Date.now() + s * 1000);

  test('DB: one job per guild per claim, none for a guild with a job running, and a crashed process\'s row does not hold a guild', { skip: skipDb }, async () => {
    const [gA, gB] = [ID(1), ID(2)];
    const cleanup = () => prisma.guildSettings.deleteMany({ where: { guildId: { in: [gA, gB] } } });
    await cleanup();
    resetTrace();
    const claim = () => scheduler.tick(clientFor([gA, gB]), { interactive: true });
    const task = (guildId, key, secondsAgo) => scheduler.scheduleJob({
      guildId, type: 'dashboard.task', runAt: inSeconds(-secondsAgo), dedupeKey: `dashboard.task:${guildId}:${key}`, maxAttempts: 1,
    });
    const status = async (id) => (await prisma.scheduledJob.findUnique({ where: { id } }))?.status ?? null;
    try {
      // Guild A has three tasks due, guild B one: a claim takes A's earliest and B's, then one of A's per claim.
      const a1 = await task(gA, 'a1', 8);
      const a2 = await task(gA, 'a2', 7);
      const a3 = await task(gA, 'a3', 6);
      const b1 = await task(gB, 'b1', 5);
      assert.equal(await claim(), 2);
      assert.deepEqual([...trace.started].sort(), [a1.id, b1.id].sort());
      assert.equal(await status(a2.id), 'PENDING');
      assert.equal(await claim(), 1);
      assert.equal(trace.started.at(-1), a2.id);
      assert.equal(await claim(), 1);
      assert.equal(trace.started.at(-1), a3.id);
      assert.equal(await claim(), 0);

      // A task of guild A is running (claimed a moment ago by a live loop): A's next task waits, B's does not.
      resetTrace();
      const c1 = await task(gA, 'c1', 4);
      const c2 = await task(gA, 'c2', 3);
      const d1 = await task(gB, 'd1', 2);
      await prisma.scheduledJob.update({ where: { id: c1.id }, data: { status: 'RUNNING', lockedBy: 'live', lockedUntil: inSeconds(300) } });
      assert.equal(await claim(), 1);
      assert.deepEqual(trace.started, [d1.id]);
      assert.equal(await status(c2.id), 'PENDING', 'guild A already has a task running');
      await prisma.scheduledJob.delete({ where: { id: c1.id } });
      assert.equal(await claim(), 1);
      assert.deepEqual(trace.started, [d1.id, c2.id], 'A\'s next task starts once the running one is gone');

      // A row a crashed process left RUNNING (claimed minutes ago, lock still valid) must not hold the guild back.
      resetTrace();
      const e1 = await task(gA, 'e1', 4);
      const e2 = await task(gA, 'e2', 3);
      await prisma.scheduledJob.update({
        where: { id: e1.id },
        data: { status: 'RUNNING', lockedBy: 'dead', lockedUntil: inSeconds(240), updatedAt: new Date(Date.now() - 10 * 60_000) },
      });
      assert.equal(await claim(), 1);
      assert.deepEqual(trace.started, [e2.id]);
    } finally {
      await cleanup();
    }
  });

  test('DB: merge changes only the payload of a live row (a RUNNING one is not reset), and settleMergedJob ends or requeues under the lock', { skip: skipDb }, async () => {
    const gid = ID(3);
    const cleanup = () => prisma.guildSettings.deleteMany({ where: { guildId: { in: [gid] } } });
    await cleanup();
    const key = `ticket.managersync:${gid}`;
    const sync = (roles) => scheduler.scheduleJob({
      guildId: gid, type: 'ticket.managersync', runAt: new Date(), dedupeKey: key, maxAttempts: 3,
      payload: managerSyncPayload('panel_db', roles), onConflict: 'merge', merge: mergeManagerSync,
    });
    const rolesIn = async () => {
      const row = await prisma.scheduledJob.findUnique({ where: { dedupeKey: key } });
      return row && [...row.payload.panels.panel_db.roles].sort();
    };
    const r = (n) => `9650000000000${String(n).padStart(5, '0')}`;
    try {
      const first = await sync([r(1)]);
      const again = await sync([r(2)]);
      assert.equal(again.id, first.id, 'one row per guild');
      assert.deepEqual(await rolesIn(), [r(1), r(2)]);
      assert.equal(await prisma.scheduledJob.count({ where: { guildId: gid } }), 1);

      // running: claimed by a live loop. Merging must leave status, version and lock alone.
      await prisma.scheduledJob.update({ where: { id: first.id }, data: { status: 'RUNNING', attempts: 1, lockedBy: 'live', lockedUntil: inSeconds(300) } });
      const running = await prisma.scheduledJob.findUnique({ where: { id: first.id } });
      await sync([r(3)]);
      const after = await prisma.scheduledJob.findUnique({ where: { id: first.id } });
      assert.equal(after.status, 'RUNNING');
      assert.equal(after.version, running.version);
      assert.equal(after.attempts, 1);
      assert.equal(after.lockedBy, 'live');
      assert.deepEqual(await rolesIn(), [r(1), r(2), r(3)]);

      // the run ends: what it did not finish (here everything) goes back to PENDING with a bumped version...
      const job = { id: first.id, version: after.version };
      assert.equal(await scheduler.settleMergedJob(job, (live) => live), 'again');
      let row = await prisma.scheduledJob.findUnique({ where: { id: first.id } });
      assert.equal(row.status, 'PENDING');
      assert.equal(row.version, after.version + 1);
      assert.equal(row.lockedBy, null);
      assert.equal(row.attempts, 0);
      // ...a stale run (the version moved) changes nothing...
      assert.equal(await scheduler.settleMergedJob(job, () => null), 'gone');
      assert.ok(await prisma.scheduledJob.findUnique({ where: { id: first.id } }));
      // ...and a run with nothing left deletes the row.
      assert.equal(await scheduler.settleMergedJob({ id: first.id, version: row.version }, () => null), 'done');
      assert.equal(await prisma.scheduledJob.findUnique({ where: { id: first.id } }), null);
      assert.equal(await rolesIn(), null);
    } finally {
      await cleanup();
    }
  });
});
