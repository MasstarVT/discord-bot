// Regression tests for three low-severity dashboard findings. Offline: fake Discord client and guild, stubbed Prisma methods,
// no Postgres or Redis.
//   1. Channel access: the saved embeds, custom commands and automations stores refuse a post, edit or delete in a channel the
//      dashboard user cannot see (a Manage Server admin whose channel overwrite hides it), as the slash commands do. The
//      check is the bot task `access.channels`, which resolves the user as a real member; owners and Administrators pass.
//   2. Login quota: a few sources sending junk callbacks cannot use up the overall allowance and lock out a clean source
//      (an IPv6 /64 counts as one source; a source's first callback in a window is free; the mark is set before the exchange).
//   3. Role pings: a saved embed's role allowlist comes from the text the admin wrote, so a member-named thread
//      (`{channel.name}`) cannot add one; free-text built-ins (channel and server name) are unmentioned as well.
import { test, describe, after } from 'node:test';
import assert from 'node:assert/strict';
import { request } from 'node:http';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Importing the stores opens the Redis and Prisma clients (nothing is sent through them): point both at a closed port.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { PermissionFlagsBits, PermissionsBitField } = await import('discord.js');
const auth = await import('../src/dashboard/auth.js');
const { loadDashboardConfig } = await import('../src/dashboard/config.js');
const {
  createDashboardServer, loginSourceKey, LOGIN_RATE_LIMIT, LOGIN_GLOBAL_RATE_LIMIT, LOGIN_FIRST_ATTEMPT_RATE_LIMIT,
} = await import('../src/dashboard/server.js');
const { RecordError } = await import('../src/dashboard/collections/errors.js');
const A = await import('../src/dashboard/collections/automations.js');
const { store: embedsStore, renderForDiscord } = await import('../src/dashboard/collections/embeds.store.js');
const { store: tagsStore } = await import('../src/dashboard/collections/tags.store.js');
const { store: automationsStore } = await import('../src/dashboard/collections/automations.store.js');
const { store: remindersStore } = await import('../src/dashboard/collections/reminders.store.js');
const { TASKS } = await import('../src/services/dashboardTasks/index.js');
const { TaskError } = await import('../src/services/dashboardTasks/shared.js');
const { renderSavedEmbed, publishSavedEmbed, extractRoleMentionIds } = await import('../src/services/embedService.js');
const { renderPlaceholders } = await import('../src/utils/placeholderParser.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');

// The logger prints through console, and the Redis client reports its failed reconnects: keep the test output clean.
const realConsole = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// Prisma methods the tests replace, put back after the run.
const patched = [];
function patch(target, key, fn) {
  patched.push([target, key, target[key]]);
  target[key] = fn;
}
after(async () => {
  for (const [target, key, original] of patched.reverse()) target[key] = original;
  Object.assign(console, realConsole);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

const bits = (...names) => new PermissionsBitField(names.map((n) => PermissionFlagsBits[n]));
const quiet = { info() {}, warn() {}, error() {}, debug() {}, success() {} };

// ── 1. Channel access ─────────────────────────────────────────────────────────────────────────────────────────────────

describe('channel access of the record stores', () => {
  const GUILD = '955040000000000001';
  const OWNER = '955040000000000002';
  const MANAGER = '955040000000000010'; // Manage Server only, no staff role
  const STAFF = '955040000000000011'; // Manage Server, with the staff role
  const ADMIN = '955040000000000012'; // Administrator
  const LEFT = '955040000000000013'; // not in the server any more
  const STAFF_ROLE = '955040000000000030';
  const HIDDEN = '955040000000000020'; // #staff-only: @everyone is denied View Channel, the staff role is allowed
  const VISIBLE = '955040000000000021';
  const HIDDEN2 = '955040000000000022';
  const GONE = '955040000000000029'; // deleted

  const members = new Map([
    [MANAGER, { id: MANAGER, roleIds: [], permissions: bits('ManageGuild') }],
    [STAFF, { id: STAFF, roleIds: [STAFF_ROLE], permissions: bits('ManageGuild') }],
    [ADMIN, { id: ADMIN, roleIds: [], permissions: bits('Administrator') }],
  ]);
  const guild = { id: GUILD, ownerId: OWNER, channels: { cache: new Map(), fetch: async () => { throw new Error('Unknown Channel'); } } };
  /** A channel that only members with the staff role can use when `hidden` (Discord's overwrite result), everyone otherwise. */
  const makeChannel = (id, name, hidden) => ({
    id, name, guild, isThread: () => false,
    permissionsFor: (m) => (!hidden || m.roleIds.includes(STAFF_ROLE) ? bits('ViewChannel', 'SendMessages', 'CreatePublicThreads') : bits()),
  });
  for (const [id, name, hidden] of [[HIDDEN, 'staff-only', true], [HIDDEN2, 'mods-only', true], [VISIBLE, 'general', false]]) {
    guild.channels.cache.set(id, makeChannel(id, name, hidden));
  }

  const taskActor = (userId) => ({ id: userId, member: members.get(userId) ?? null, isBotOwner: false });
  const run = (userId, payload, actor = taskActor(userId)) => TASKS['access.channels'].run({ guild, client: {}, actor, payload, taskId: 't' });
  const check = (channelId, extra = {}) => ({ channelId, ...extra });

  describe('the bot task', () => {
    test('is registered, and refuses a hidden channel for a Manage Server member but not for staff, Administrators or the owner', async () => {
      assert.equal(typeof TASKS['access.channels']?.run, 'function');
      const hidden = await run(MANAGER, { checks: [check(HIDDEN, { needs: ['ViewChannel', 'SendMessages'] })] });
      assert.equal(hidden.data.problems.length, 1);
      assert.deepEqual(hidden.data.problems[0].index, 0);
      assert.match(hidden.data.problems[0].text, /View Channel, Send Messages in #staff-only/);
      assert.match(hidden.message, /staff-only/);
      for (const who of [STAFF, ADMIN]) assert.deepEqual((await run(who, { checks: [check(HIDDEN)] })).data.problems, [], who);
      assert.deepEqual((await run(OWNER, { checks: [check(HIDDEN)] }, { id: OWNER, member: { id: OWNER, roleIds: [], permissions: bits() }, isBotOwner: false })).data.problems, []);
      assert.deepEqual((await run(MANAGER, { checks: [check(VISIBLE)] })).data.problems, []);
    });

    test('lists every failed check by index, and takes the default needs', async () => {
      const out = await run(MANAGER, { checks: [check(VISIBLE), check(HIDDEN), check(VISIBLE), check(HIDDEN2, { needs: ['ViewChannel'] })] });
      assert.deepEqual(out.data.problems.map((p) => p.index), [1, 3]);
      assert.match(out.data.problems[1].text, /View Channel in #mods-only/);
    });

    test('a member who left is refused (the bot owner is not), and a missing channel only matters when it is required', async () => {
      await assert.rejects(run(LEFT, { checks: [check(VISIBLE)] }), (err) => err instanceof TaskError && /member of this server/.test(err.message));
      const owner = await run(LEFT, { checks: [check(HIDDEN)] }, { id: LEFT, member: null, isBotOwner: true });
      assert.deepEqual(owner.data.problems, []);
      assert.deepEqual((await run(MANAGER, { checks: [check(GONE, { optional: true })] })).data.problems, []);
      assert.match((await run(MANAGER, { checks: [check(GONE)] })).data.problems[0].text, /cannot find that channel/);
    });

    test('re-validates its payload', async () => {
      for (const payload of [null, {}, { checks: [] }, { checks: Array(26).fill(check(VISIBLE)) }, { checks: [check('nope')] },
        { checks: [check(VISIBLE, { needs: ['Administrator'] })] }, { checks: [check(VISIBLE, { needs: [] })] }, { checks: ['x'] }]) {
        await assert.rejects(run(MANAGER, payload), (err) => err instanceof TaskError, JSON.stringify(payload));
      }
    });
  });

  // The store context the records bridge builds, with `runTask` answered by the real task.
  function ctxFor(userId, member = { owner: false, permissions: String(PermissionFlagsBits.ManageGuild) }, { bot = true } = {}) {
    const calls = { tasks: [], send: [], edit: [], del: [] };
    const discord = {
      async getGuildResources() {
        return {
          guild: { id: GUILD, name: 'Guild', ownerId: OWNER, memberCount: 3 },
          channels: [HIDDEN, HIDDEN2, VISIBLE].map((id) => ({ id, name: guild.channels.cache.get(id).name, type: 0, kind: 'text' })),
          roles: [],
        };
      },
      async sendMessage(channelId, body) { calls.send.push([channelId, body]); return { id: `955040000000009${String(calls.send.length).padStart(3, '0')}`, channelId }; },
      async editMessage(channelId, messageId) { calls.edit.push([channelId, messageId]); },
      async deleteMessage(channelId, messageId) { calls.del.push([channelId, messageId]); },
    };
    const ctx = { guildId: GUILD, userId, user: { id: userId, username: 'user', globalName: null, avatar: null }, member, discord, logger: quiet };
    if (bot) {
      ctx.runTask = async (kind, payload) => {
        calls.tasks.push([kind, payload]);
        try {
          const out = await TASKS[kind].run({ guild, client: {}, actor: taskActor(userId), payload, taskId: 't' });
          return { status: 'done', message: out.message, data: out.data ?? null };
        } catch (err) {
          if (err instanceof TaskError) return { status: 'failed', message: err.message, data: null };
          throw err;
        }
      };
    }
    return { ctx, calls };
  }
  const refused = (promise, path, re = /View Channel/) => assert.rejects(promise, (err) => {
    assert.ok(err instanceof RecordError, err.stack);
    assert.equal(err.code, 'invalid', err.message); // an HTTP 400
    assert.equal(err.status, 400);
    assert.equal(err.path, path, err.message);
    assert.match(err.message, re);
    return true;
  });
  const GREAT = Object.freeze({ owner: false, permissions: String(PermissionFlagsBits.Administrator) });

  describe('saved embeds', () => {
    const rows = new Map();
    const seed = (over = {}) => {
      const row = { id: `e${rows.size + 1}`, guildId: GUILD, name: `emb${rows.size + 1}`, template: { content: 'Hello' }, channelId: null, messageId: null, createdBy: MANAGER, updatedAt: new Date(), ...over };
      rows.set(row.id, row);
      return row;
    };
    const writes = [];
    patch(prisma.savedEmbed, 'findFirst', async ({ where }) => (rows.get(where.id)?.guildId === where.guildId ? { ...rows.get(where.id) } : null));
    patch(prisma.savedEmbed, 'updateMany', async ({ where, data }) => {
      const row = rows.get(where.id);
      if (!row) return { count: 0 };
      writes.push(data);
      Object.assign(row, data);
      return { count: 1 };
    });
    patch(prisma.savedEmbed, 'deleteMany', async ({ where }) => { writes.push('delete'); return { count: rows.delete(where.id) ? 1 : 0 }; });
    patch(prisma.guildSettings, 'findUnique', async ({ where }) => ({ guildId: where.guildId }));
    let automationRows = []; // the automations of the server, as the store reads them (only their actions)
    patch(prisma.automation, 'findMany', async () => automationRows);

    test('publish: a hidden channel is refused before anything is sent, a visible one is posted', async () => {
      const row = seed();
      const { ctx, calls } = ctxFor(MANAGER);
      writes.length = 0;
      await refused(embedsStore.actions.publish(GUILD, row.id, { channelId: HIDDEN, deletePrevious: true }, ctx), 'channelId', /View Channel, Send Messages in #staff-only/);
      assert.deepEqual([calls.send.length, writes.length], [0, 0], 'nothing sent, nothing saved');
      const out = await embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE, deletePrevious: true }, ctx);
      assert.equal(calls.send.length, 1);
      assert.equal(calls.send[0][0], VISIBLE);
      assert.match(out.message, /Posted in #general/);
      assert.equal(rows.get(row.id).channelId, VISIBLE);
    });

    test('publish: moving a copy out of a hidden channel needs View there (deleting it) unless the old copy is kept', async () => {
      const row = seed({ channelId: HIDDEN, messageId: '955040000000008001' });
      const { ctx, calls } = ctxFor(MANAGER);
      await refused(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE }, ctx), 'deletePrevious', /View Channel in #staff-only/);
      await refused(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE, deletePrevious: true }, ctx), 'deletePrevious');
      assert.deepEqual([calls.send.length, calls.del.length], [0, 0], 'the hidden copy is untouched');
      await embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE, deletePrevious: false }, ctx);
      assert.deepEqual([calls.send.length, calls.del.length], [1, 0], 'kept: only the new copy is posted');
    });

    test('update, refresh, unpublish and delete of a copy in a hidden channel are refused', async () => {
      const { ctx, calls } = ctxFor(MANAGER);
      const posted = () => seed({ channelId: HIDDEN, messageId: '955040000000008002' });
      writes.length = 0;
      let row = posted();
      await refused(embedsStore.update(GUILD, row.id, { template: { content: 'Changed' } }, ctx), 'template');
      await refused(embedsStore.actions.refresh(GUILD, row.id, {}, ctx), undefined);
      await refused(embedsStore.actions.unpublish(GUILD, row.id, {}, ctx), undefined);
      await refused(embedsStore.remove(GUILD, row.id, ctx), undefined);
      assert.deepEqual([calls.edit.length, calls.del.length, writes.length], [0, 0, 0]);
      assert.ok(rows.has(row.id), 'the record is still there');
      assert.deepEqual(rows.get(row.id).template, { content: 'Hello' });
      // A copy in a visible channel is fine.
      row = seed({ channelId: VISIBLE, messageId: '955040000000008003' });
      await embedsStore.update(GUILD, row.id, { template: { content: 'Changed' } }, ctx);
      await embedsStore.actions.refresh(GUILD, row.id, {}, ctx);
      await embedsStore.actions.unpublish(GUILD, row.id, {}, ctx);
      assert.equal(calls.edit.length, 2);
      assert.equal(calls.del.length, 1);
      await embedsStore.remove(GUILD, row.id, ctx);
      assert.ok(!rows.has(row.id));
    });

    test('staff with the channel, Administrators and the owner may use it; a draft needs no channel check', async () => {
      const hidden = seed();
      const staff = ctxFor(STAFF);
      await embedsStore.actions.publish(GUILD, hidden.id, { channelId: HIDDEN, deletePrevious: true }, staff.ctx);
      assert.equal(staff.calls.send.length, 1);
      // Administrators and owners are exempt locally: the bot is never asked (and need not be there).
      for (const member of [GREAT, { owner: true, permissions: '0' }]) {
        const { ctx, calls } = ctxFor(MANAGER, member, { bot: false });
        const row = seed();
        await embedsStore.actions.publish(GUILD, row.id, { channelId: HIDDEN, deletePrevious: true }, ctx);
        assert.equal(calls.send.length, 1);
        assert.equal(calls.tasks.length, 0);
      }
      const { ctx, calls } = ctxFor(MANAGER);
      const draft = seed();
      await embedsStore.update(GUILD, draft.id, { template: { content: 'Draft change' } }, ctx);
      await embedsStore.remove(GUILD, draft.id, ctx);
      assert.equal(calls.tasks.length, 0, 'a draft has no channel');
    });

    test('an unposted embed that an automation posts in a hidden channel cannot be edited from outside that channel', async () => {
      const draft = seed({ name: 'rules-embed' });
      const sendEmbed = (embedName, channelId) => ({ actions: [{ type: 'REPLY', content: 'x' }, { type: 'SEND_EMBED', embedName, channelId }] });
      const edit = (ctx) => embedsStore.update(GUILD, draft.id, { template: { content: 'Rewritten' } }, ctx);
      const { ctx, calls } = ctxFor(MANAGER);
      automationRows = [sendEmbed('rules-embed', HIDDEN)];
      await refused(edit(ctx), 'template', /An automation posts this embed there.*View Channel, Send Messages in #staff-only/);
      assert.deepEqual(rows.get(draft.id).template, { content: 'Hello' }, 'nothing was saved');
      await edit(ctxFor(STAFF).ctx); // staff can use the channel
      await edit(ctxFor(MANAGER, GREAT, { bot: false }).ctx); // Administrators are exempt without asking
      // Other embeds, channels the user can use and automations without a fixed channel are not in the way.
      for (const rowsOf of [[sendEmbed('another', HIDDEN)], [sendEmbed('rules-embed', VISIBLE)], [sendEmbed('rules-embed', null)]]) {
        automationRows = rowsOf;
        await edit(ctx);
      }
      assert.equal(calls.tasks.length, 2, 'asked for the hidden and the visible channel only; the other cases had no channel to check');
    });

    test('without the bot, or when the user left the server, the write is refused rather than allowed', async () => {
      const row = seed();
      const noBot = ctxFor(MANAGER, undefined, { bot: false });
      await assert.rejects(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE }, noBot.ctx), (err) => err.code === 'unavailable');
      const left = ctxFor(LEFT);
      await assert.rejects(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE }, left.ctx), (err) => err.code === 'failed' && /member of this server/.test(err.message));
      const slow = ctxFor(MANAGER);
      slow.ctx.runTask = async () => ({ status: 'pending', message: 'still working', data: null });
      await assert.rejects(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE }, slow.ctx), (err) => err.code === 'unavailable');
      const offline = ctxFor(MANAGER);
      offline.ctx.runTask = async () => ({ status: 'offline', message: 'nobody picked it up', data: null });
      await assert.rejects(embedsStore.actions.publish(GUILD, row.id, { channelId: VISIBLE }, offline.ctx), (err) => err.code === 'unavailable');
      for (const c of [noBot, left, slow, offline]) assert.equal(c.calls.send.length, 0);
    });
  });

  // The write itself is replaced by a sentinel: a request that passes the access check reaches it, a refused one never does.
  const SENTINEL = new Error('reached the database write');
  let transactions = 0;
  patch(prisma, '$transaction', async () => { transactions += 1; throw SENTINEL; });
  patch(prisma.guildSettings, 'upsert', async () => ({}));
  const reachesWrite = async (promise) => {
    const before = transactions;
    await assert.rejects(promise, (err) => err === SENTINEL);
    assert.equal(transactions, before + 1);
  };
  const neverWrites = async (promise, path, re) => {
    const before = transactions;
    await refused(promise, path, re);
    assert.equal(transactions, before, 'the write was not attempted');
  };

  describe('custom commands', () => {
    const send = (channelId, extra = {}) => ({ type: 'send', channelId, embed: false, responses: [{ text: 'hi' }], roleId: null, pingRoleIds: [], ...extra });
    const reply = { type: 'reply', channelId: null, embed: false, responses: [{ text: 'hi' }], roleId: null, pingRoleIds: [] };
    const tag = (actions) => ({
      name: 'rules', description: null, enabled: true, actions, allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [],
      deleteInvocation: false, cooldownScope: 'NONE', cooldownSeconds: 0,
    });

    test('a send action to a hidden channel is refused at its field; a visible channel and plain replies pass', async () => {
      const { ctx, calls } = ctxFor(MANAGER);
      await neverWrites(tagsStore.create(GUILD, tag([reply, send(HIDDEN)]), ctx), 'actions.1.channelId', /View Channel, Send Messages in #staff-only/);
      await reachesWrite(tagsStore.create(GUILD, tag([send(VISIBLE)]), ctx));
      const before = calls.tasks.length;
      await reachesWrite(tagsStore.create(GUILD, tag([reply]), ctx));
      assert.equal(calls.tasks.length, before, 'no channel, no check');
      await reachesWrite(tagsStore.create(GUILD, tag([send(HIDDEN)]), ctxFor(STAFF).ctx));
      await reachesWrite(tagsStore.create(GUILD, tag([send(HIDDEN)]), ctxFor(MANAGER, GREAT, { bot: false }).ctx));
    });

    test('an edit keeps a send action exactly as it was, but a changed or added one is checked even for a stored channel', async () => {
      const stored = { id: 'c1', guildId: GUILD, name: 'rules', description: null, enabled: true, createdBy: OWNER, updatedAt: new Date(),
        actions: [{ type: 'send', channelId: HIDDEN, embed: false, responses: ['hi'] }], content: '', useEmbed: false, cooldownScope: 'NONE', cooldownSeconds: 0, uses: 0,
        allowRoleIds: [], denyRoleIds: [], allowChannelIds: [], denyChannelIds: [], deleteInvocation: false, createdAt: new Date() };
      patch(prisma.customTag, 'findFirst', async () => ({ ...stored }));
      patch(prisma.customTag, 'updateMany', async () => ({ count: 1 }));
      const { ctx, calls } = ctxFor(MANAGER);
      await tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN), reply] }, ctx);
      assert.equal(calls.tasks.length, 0, 'the stored action, as it was, is kept without a check (a reply added next to it too)');
      // The channel was already the command's, but a lower-ranked admin must not write to it.
      await refused(tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN, { responses: [{ text: 'changed' }] })] }, ctx), 'actions.0.channelId', /#staff-only/);
      await refused(tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN, { embed: true })] }, ctx), 'actions.0.channelId');
      await refused(tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN), send(HIDDEN)] }, ctx), 'actions.1.channelId', /#staff-only/);
      await refused(tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN), send(HIDDEN2)] }, ctx), 'actions.1.channelId', /#mods-only/);
      await tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN, { responses: [{ text: 'changed' }] })] }, ctxFor(STAFF).ctx);
      await tagsStore.update(GUILD, 'c1', { actions: [send(HIDDEN, { responses: [{ text: 'changed' }] })] }, ctxFor(MANAGER, GREAT, { bot: false }).ctx);
    });
  });

  describe('reminder test posts', () => {
    const reminder = { id: 'r1', guildId: GUILD, name: 'standup', channelId: VISIBLE, template: { content: 'Standup time' }, mode: 'ONE_SHOT',
      timeOfDay: null, weekdays: [], intervalMinutes: null, windowStart: null, windowEnd: null, runAt: null, requireActivity: false,
      pingRoleId: null, enabled: false, lastSentAt: null };
    patch(prisma.reminder, 'findFirst', async () => ({ ...reminder }));

    test('the test action posts only in a channel the user can post in, whichever channel the form names', async () => {
      const { ctx, calls } = ctxFor(MANAGER);
      await refused(remindersStore.actions.test(GUILD, 'r1', { channelId: HIDDEN }, ctx), 'channelId', /View Channel, Send Messages in #staff-only/);
      assert.equal(calls.send.length, 0, 'nothing was posted');
      const stored = await remindersStore.actions.test(GUILD, 'r1', {}, ctx);
      assert.match(stored.message, /#general/);
      assert.deepEqual(calls.send.map(([channelId]) => channelId), [VISIBLE]);
      const staff = ctxFor(STAFF);
      await remindersStore.actions.test(GUILD, 'r1', { channelId: HIDDEN }, staff.ctx);
      assert.deepEqual(staff.calls.send.map(([channelId]) => channelId), [HIDDEN]);
      const admin = ctxFor(MANAGER, GREAT, { bot: false });
      await remindersStore.actions.test(GUILD, 'r1', { channelId: HIDDEN }, admin.ctx);
      assert.deepEqual(admin.calls.send.map(([channelId]) => channelId), [HIDDEN]);
    });
  });

  describe('automations', () => {
    const action = (type, extra = {}) => ({ ...A.blankAction(type), ...extra });
    const create = (actions, extra = {}) => ({ name: 'watch', trigger: 'MESSAGE_SENT', conditions: [], actions, enabled: true, cooldownSeconds: 0, ...extra });

    test('send, embed, repost and thread targets are refused when hidden; other actions and visible channels pass', async () => {
      const { ctx, calls } = ctxFor(MANAGER);
      patch(prisma.automation, 'findMany', async () => []);
      await neverWrites(automationsStore.create(GUILD, create([action('SEND_MESSAGE', { content: 'hi', channelId: HIDDEN })]), ctx),
        'actions.0.channelId', /Send Messages in #staff-only/);
      await neverWrites(automationsStore.create(GUILD, create([action('REPLY', { content: 'ok' }), action('REPOST', { channelId: HIDDEN })]), ctx),
        'actions.1.channelId');
      await neverWrites(automationsStore.create(GUILD, create([action('CREATE_THREAD', { threadChannelId: HIDDEN })]), ctx),
        'actions.0.threadChannelId');
      const before = calls.tasks.length;
      await reachesWrite(automationsStore.create(GUILD, create([action('REPLY', { content: 'ok' })]), ctx));
      assert.equal(calls.tasks.length, before, 'no channel, no check');
      await reachesWrite(automationsStore.create(GUILD, create([action('SEND_MESSAGE', { content: 'hi', channelId: VISIBLE })]), ctx));
      await reachesWrite(automationsStore.create(GUILD, create([action('SEND_MESSAGE', { content: 'hi', channelId: HIDDEN })]), ctxFor(STAFF).ctx));
      await reachesWrite(automationsStore.create(GUILD, create([action('SEND_MESSAGE', { content: 'hi', channelId: HIDDEN })]), ctxFor(MANAGER, GREAT, { bot: false }).ctx));
    });

    test('a preset that posts in a hidden channel is refused at the preset channel', async () => {
      const { ctx } = ctxFor(MANAGER);
      const welcomer = (channel) => create([], { preset: 'welcomer', presetChannelId: channel, presetMessage: 'Welcome {user}', presetRoleId: null });
      await neverWrites(automationsStore.create(GUILD, welcomer(HIDDEN), ctx), 'presetChannelId');
      await reachesWrite(automationsStore.create(GUILD, welcomer(VISIBLE), ctx));
    });

    test('an edit keeps an action exactly as it was, but a changed or added one is checked even for a stored channel', async () => {
      const stored = { id: 'a1', guildId: GUILD, name: 'watch', trigger: 'MESSAGE_SENT', conditions: [], enabled: true, cooldownSeconds: 0, buttonSeq: 0,
        createdBy: OWNER, createdAt: new Date(), updatedAt: new Date(),
        actions: [{ type: 'SEND_MESSAGE', content: 'hi', channelId: HIDDEN, button: null }, { type: 'CREATE_THREAD', name: 'talk', channelId: HIDDEN2, autoArchiveMinutes: 1440 }] };
      patch(prisma.automation, 'findFirst', async () => ({ ...stored }));
      const { ctx, calls } = ctxFor(MANAGER);
      const same = [action('SEND_MESSAGE', { content: 'hi', channelId: HIDDEN }), action('CREATE_THREAD', { threadName: 'talk', threadChannelId: HIDDEN2 })];
      await reachesWrite(automationsStore.update(GUILD, 'a1', { actions: [...same, action('REPLY', { content: 'ok' })] }, ctx));
      assert.equal(calls.tasks.length, 0, 'unchanged actions are kept without a check (a reply added next to them too)');
      // The channels were already the automation's, but a lower-ranked admin must not write to them.
      await neverWrites(automationsStore.update(GUILD, 'a1', { actions: [action('SEND_MESSAGE', { content: 'changed', channelId: HIDDEN }), same[1]] }, ctx),
        'actions.0.channelId', /#staff-only/);
      await neverWrites(automationsStore.update(GUILD, 'a1', { actions: [...same, action('SEND_MESSAGE', { content: 'again', channelId: HIDDEN })] }, ctx),
        'actions.2.channelId', /#staff-only/);
      await neverWrites(automationsStore.update(GUILD, 'a1', { actions: [same[0], action('REPOST', { channelId: HIDDEN2 })] }, ctx),
        'actions.1.channelId', /#mods-only/);
      await neverWrites(automationsStore.update(GUILD, 'a1', { actions: [same[0], action('CREATE_THREAD', { threadName: 'renamed', threadChannelId: HIDDEN2 })] }, ctx),
        'actions.1.threadChannelId', /#mods-only/);
      await neverWrites(automationsStore.update(GUILD, 'a1', { actions: [action('SEND_MESSAGE', { content: 'hi', channelId: HIDDEN2 })] }, ctx),
        'actions.0.channelId', /#mods-only/);
      await reachesWrite(automationsStore.update(GUILD, 'a1', { actions: [action('SEND_MESSAGE', { content: 'changed', channelId: HIDDEN })] }, ctxFor(STAFF).ctx));
      await reachesWrite(automationsStore.update(GUILD, 'a1', { actions: [action('SEND_MESSAGE', { content: 'changed', channelId: HIDDEN })] }, ctxFor(MANAGER, GREAT, { bot: false }).ctx));
    });
  });
});

// ── 2. Login quota ────────────────────────────────────────────────────────────────────────────────────────────────────

describe('GET /auth/callback: the overall login allowance', () => {
  const { config } = loadDashboardConfig({
    CLIENT_ID: '123456789012345678', DISCORD_TOKEN: 'bot-token', DISCORD_CLIENT_SECRET: 'client-secret',
    DASHBOARD_SESSION_SECRET: 'x'.repeat(48), DATABASE_URL: 'postgresql://u:p@localhost/db', REDIS_URL: 'redis://localhost:6379',
  });
  const callbackRequest = (port, address, code) => new Promise((resolvePromise, reject) => {
    const req = request({
      host: '127.0.0.1', port, method: 'GET', path: `/auth/callback?code=${code}&state=abc`, agent: false,
      headers: { cookie: `${auth.STATE_COOKIE}=abc`, 'x-forwarded-for': address },
    }, (res) => {
      res.resume();
      res.on('end', () => resolvePromise({ status: res.statusCode, location: res.headers.location }));
    });
    req.on('error', reject);
    req.end();
  });
  const listen = (server) => new Promise((res) => server.listen(0, '127.0.0.1', () => res(server.address().port)));
  const close = (server) => new Promise((res) => { server.close(() => res()); server.closeAllConnections(); });

  /** A dashboard whose token exchange fails for every code but `legit_code` (and parks `slow*` codes until released). */
  async function harness() {
    const h = { clock: 1_800_000_000_000, exchanges: [], gate: null };
    h.gate = new Promise((res) => { h.release = res; });
    const discord = {
      async exchangeCode(code) {
        h.exchanges.push(code);
        if (code.startsWith('slow')) await h.gate;
        if (code === 'legit_code') return { accessToken: 'token', expiresIn: 3600 };
        throw new Error('token exchange returned HTTP 400');
      },
      async getCurrentUser() { return { id: '955050000000000001', username: 'admin', globalName: null, avatar: null }; },
    };
    h.server = createDashboardServer({ config, deps: { discord, settings: {}, now: () => h.clock }, logger: quiet });
    h.port = await listen(h.server);
    h.call = (address, code = 'junk') => callbackRequest(h.port, address, code);
    h.close = () => close(h.server);
    return h;
  }
  const waitFor = async (condition) => {
    for (let i = 0; i < 200 && !condition(); i += 1) await new Promise((res) => setTimeout(res, 25));
    assert.ok(condition(), 'timed out');
  };
  const LEGIT_OK = { status: 302, location: '/dashboard' };

  test('many sources sending junk cannot lock out a clean source (six addresses of 10, then ten of 10)', async () => {
    const h = await harness();
    try {
      for (let s = 10; s < 16; s += 1) for (let i = 0; i < LOGIN_RATE_LIMIT.max; i += 1) await h.call(`198.51.100.${s}`);
      assert.equal(h.exchanges.length, 60, 'the attack of the finding: 6 x 10 junk callbacks');
      assert.deepEqual(await h.call('203.0.113.77', 'legit_code'), LEGIT_OK, 'the clean source is not refused');
      assert.equal(h.exchanges.at(-1), 'legit_code');

      for (let s = 20; s < 30; s += 1) for (let i = 0; i < LOGIN_RATE_LIMIT.max; i += 1) await h.call(`198.51.100.${s}`);
      assert.ok(h.exchanges.length - 61 <= LOGIN_GLOBAL_RATE_LIMIT.max + 10, 'the flood is still bounded: 60 plus one free callback per source');
      assert.deepEqual(await h.call('203.0.113.78', 'legit_code'), LEGIT_OK, 'still not refused once the overall allowance is gone');
    } finally {
      await h.close();
    }
  });

  test('an IPv6 /64 is one source: its addresses share the per-source cap, and cannot starve another network', async () => {
    const h = await harness();
    try {
      const results = [];
      for (let i = 0; i < 30; i += 1) results.push((await h.call(`2001:db8:1:2::${(i + 10).toString(16)}`)).status);
      assert.equal(results.filter((s) => s === 429).length, 20, '10 callbacks for the whole /64 a minute');
      assert.equal(h.exchanges.length, LOGIN_RATE_LIMIT.max);
      assert.equal((await h.call('2001:DB8:1:2:abcd::1')).status, 429, 'same /64, written another way');
      assert.deepEqual(await h.call('2001:db8:9999::1', 'legit_code'), LEGIT_OK, 'another /64 is unaffected');
      assert.equal((await h.call('2001:db8:1:3::1')).status, 302, 'the next /64 is a different source');
      h.clock += LOGIN_RATE_LIMIT.windowMs + 1;
      assert.equal((await h.call('2001:db8:1:2::99')).status, 302, 'and the /64 is back after its minute');
    } finally {
      await h.close();
    }
  });

  test('a source\'s first callback is free; its next, while that has not succeeded, spends the allowance', async () => {
    const h = await harness();
    try {
      const max = LOGIN_GLOBAL_RATE_LIMIT.max;
      for (let i = 0; i <= max; i += 1) assert.equal((await h.call(`203.0.113.${i}`)).status, 302, `first callback of source ${i}`);
      assert.equal(h.exchanges.length, max + 1, 'every first callback reached Discord');
      for (let i = 0; i < max; i += 1) assert.equal((await h.call(`203.0.113.${i}`)).status, 302, `second callback of source ${i}`);
      assert.equal((await h.call(`203.0.113.${max}`)).status, 429, 'the 61st second callback finds the allowance gone');
      assert.equal(h.exchanges.length, 2 * max + 1, 'and never reached Discord');
      h.clock += LOGIN_GLOBAL_RATE_LIMIT.windowMs + 1;
      assert.equal((await h.call(`203.0.113.${max}`)).status, 302);
    } finally {
      await h.close();
    }
  });

  test('a successful login clears the mark, so the next callback of that source is free again', async () => {
    const h = await harness();
    try {
      for (let i = 0; i < 60; i += 1) await h.call(`203.0.113.${i}`); // 60 first callbacks: free
      for (let i = 0; i < 60; i += 1) await h.call(`203.0.113.${i}`); // 60 second callbacks: the allowance is gone now
      assert.equal((await h.call('198.51.100.9')).status, 302, 'a fresh source is free');
      assert.equal((await h.call('198.51.100.9')).status, 429, 'but its next callback is not');
      h.clock += LOGIN_GLOBAL_RATE_LIMIT.windowMs + 1;
      assert.deepEqual(await h.call('198.51.100.10', 'legit_code'), LEGIT_OK);
      assert.equal((await h.call('198.51.100.10')).status, 302, 'junk after a success is a first callback again');
      assert.equal((await h.call('198.51.100.10')).status, 302);
    } finally {
      await h.close();
    }
  });

  test('concurrent callbacks of a fresh source cannot all skip the allowance (the mark comes before the exchange)', async () => {
    const h = await harness();
    try {
      const max = LOGIN_GLOBAL_RATE_LIMIT.max;
      const sources = 7;
      const pending = [];
      for (let s = 0; s < sources; s += 1) for (let i = 0; i < LOGIN_RATE_LIMIT.max; i += 1) pending.push(h.call(`198.51.100.${40 + s}`, `slow${s}x${i}`));
      // 7 first callbacks are free and 63 spend the allowance of 60: three are refused before they reach Discord.
      await waitFor(() => h.exchanges.length === sources + max);
      await new Promise((res) => setTimeout(res, 100));
      assert.equal(h.exchanges.length, sources + max, 'if every concurrent request of a fresh source skipped the allowance, all 70 would get through');
      h.release();
      const statuses = (await Promise.all(pending)).map((r) => r.status);
      assert.equal(statuses.filter((s) => s === 429).length, sources * LOGIN_RATE_LIMIT.max - (sources + max));
    } finally {
      h.release();
      await h.close();
    }
  });

  test('many sources cannot flood Discord either: first callbacks have their own, larger ceiling', async () => {
    assert.deepEqual({ ...LOGIN_FIRST_ATTEMPT_RATE_LIMIT }, { max: 600, windowMs: 60_000 });
    const h = await harness();
    try {
      const max = LOGIN_FIRST_ATTEMPT_RATE_LIMIT.max;
      // One address in each of many IPv6 /64s (a /48 holds 65,536 of them): every one is a fresh source, free of the 60.
      const fresh = (i) => `2001:db8:${(i + 1).toString(16)}::1`;
      for (let i = 0; i < max; i += 1) assert.equal((await h.call(fresh(i))).status, 302, `source ${i + 1}`);
      assert.equal(h.exchanges.length, max);
      const refused = await h.call(fresh(max));
      assert.equal(refused.status, 429, 'past the ceiling a fresh source is refused too');
      assert.equal(h.exchanges.length, max, 'and never reaches Discord');
      assert.equal((await h.call(fresh(0))).status, 302, 'a source that already had its first callback is not held to the ceiling (it spends the overall 60)');
      h.clock += LOGIN_FIRST_ATTEMPT_RATE_LIMIT.windowMs + 1;
      assert.deepEqual(await h.call(fresh(max), 'legit_code'), LEGIT_OK, 'and it is only a minute');
    } finally {
      await h.close();
    }
  });

  test('a port or zone id in X-Forwarded-For does not make a new source per connection', async () => {
    const h = await harness();
    try {
      const statuses = [];
      for (let i = 0; i < 12; i += 1) {
        const forms = [`198.51.100.50:${40000 + i}`, `[2001:db8:7:7::${i + 1}]:${50000 + i}`, `2001:db8:8:8::${i + 1}%eth0`];
        statuses.push(...await Promise.all(forms.map((address) => h.call(address))));
      }
      const byForm = (n) => statuses.filter((_, i) => i % 3 === n).map((r) => r.status);
      for (const n of [0, 1, 2]) assert.deepEqual(byForm(n).filter((s) => s === 429).length, 2, `form ${n}: 10 callbacks a minute, then 429`);
      assert.equal(h.exchanges.length, 30, 'three sources, ten exchanges each');
      assert.deepEqual(await h.call('[::ffff:198.51.100.50]:6000', 'legit_code').then((r) => r.status), 429, 'IPv4-mapped with a port is the same source as 198.51.100.50');
    } finally {
      await h.close();
    }
  });

  test('loginSourceKey: an IPv6 address is its /64, IPv4 and everything else are kept', () => {
    const key = (a) => loginSourceKey(a);
    assert.equal(key('198.51.100.7:51234'), '198.51.100.7', 'a port is dropped');
    assert.equal(key('[2001:db8:1:2::a]:443'), key('2001:db8:1:2::b'));
    assert.equal(key('[2001:db8:1:2::a]'), key('2001:db8:1:2::b'));
    assert.equal(key('[fe80::1%eth0]:80'), key('fe80::9'), 'a zone id behind brackets and a port');
    assert.equal(key('[::ffff:203.0.113.9]:5000'), '203.0.113.9');
    assert.equal(key('::ffff:203.0.113.9'), '203.0.113.9');
    assert.equal(key('::FFFF:203.0.113.9%eth0'), '203.0.113.9');
    assert.equal(key('2001:db8:1:2::a'), key('2001:DB8:1:2:0:0:0:f'));
    assert.equal(key('2001:db8:1:2::a'), key('2001:db8:1:2:ffff:ffff:ffff:ffff'));
    assert.notEqual(key('2001:db8:1:2::a'), key('2001:db8:1:3::a'));
    assert.equal(key('2001:db8:1:2::a'), '2001:0db8:0001:0002::/64');
    assert.equal(key('::1'), '0000:0000:0000:0000::/64');
    assert.equal(key('fe80::1%eth0'), key('fe80::2'), 'a zone id is dropped');
    assert.equal(key('::ffff:203.0.113.9'), '203.0.113.9', 'an IPv4-mapped address is its IPv4 address');
    assert.equal(key('::ffff:cb00:7109'), '203.0.113.9');
    assert.equal(key('203.0.113.9'), '203.0.113.9');
    assert.equal(key('unknown'), 'unknown');
    assert.equal(key('not an ip'), 'not an ip');
    assert.equal(key(undefined), '');
  });
});

// ── 3. Role pings of a saved embed ────────────────────────────────────────────────────────────────────────────────────

describe('saved embed role pings come from the text the admin wrote', () => {
  const GUILD = '955060000000000001';
  const ROLE = '777777777777777777';
  const OWNER_ID = '955060000000000002';
  const PUBLISHER = '955060000000000003';
  const BOT = '955060000000000004';
  const THREAD = '955060000000000005';
  const NAME = `<@&${ROLE}>`; // a thread name a member chose

  const MENTION_ALL = new PermissionsBitField(PermissionsBitField.All);
  const guild = {
    id: GUILD, name: 'Guild', available: true, ownerId: OWNER_ID, memberCount: 3, iconURL: () => null,
    client: { user: { id: BOT } },
    members: { me: { id: BOT, permissions: MENTION_ALL } },
    roles: { cache: new Map([[ROLE, { id: ROLE, mentionable: false }]]), everyone: { id: GUILD } },
  };
  const member = (perms) => ({
    id: PUBLISHER, guild, displayName: 'Publisher', permissions: bits('ManageGuild'),
    user: { id: PUBLISHER, username: 'publisher', bot: false, displayAvatarURL: () => 'https://cdn.example/a.png' }, _perms: perms,
  });
  function thread(name = NAME) {
    const sent = [];
    return {
      id: THREAD, name, type: 11, guild, sent,
      isThread: () => true,
      isTextBased: () => true,
      permissionsFor: (who) => (who.id === BOT ? MENTION_ALL : who._perms),
      async send(payload) { sent.push(payload); return { id: '955060000000000099' }; },
    };
  }
  const NO_MENTION = new PermissionsBitField([PermissionFlagsBits.ViewChannel, PermissionFlagsBits.SendMessagesInThreads]);

  test('the finding: "New in {channel.name}" in a thread named after a role mention allows no role', async () => {
    patch(prisma.savedEmbed, 'findUnique', async ({ where }) => ({ id: where.id, template: { content: 'New in {channel.name}' }, messageId: null, channelId: null }));
    patch(prisma.savedEmbed, 'update', async () => ({}));
    const row = { id: 'sav1', name: 'news' };
    for (const perms of [MENTION_ALL, NO_MENTION]) {
      const channel = thread();
      const out = await publishSavedEmbed({ row, channel, member: member(perms) });
      assert.equal(out.ok, true);
      assert.deepEqual(out.refused, [], 'the admin typed no role, so nothing is refused either');
      assert.deepEqual(channel.sent[0].allowedMentions.roles, [], 'no role may be pinged');
      assert.match(channel.sent[0].content, /^New in </);
      assert.ok(!extractRoleMentionIds(channel.sent[0].content).length, 'and the name no longer reads as a role mention');
    }
  });

  test('a role the admin typed is still allowed (or refused) by the usual rule', async () => {
    const tpl = { content: `Ping <@&${ROLE}> about {channel.name}` };
    const allowed = renderSavedEmbed(tpl, { member: member(MENTION_ALL), guild, channel: thread() }, { mentions: 'publish' });
    assert.deepEqual(allowed.allowedMentions, { parse: ['users'], roles: [ROLE] });
    const refused = renderSavedEmbed(tpl, { member: member(NO_MENTION), guild, channel: thread() }, { mentions: 'publish' });
    assert.deepEqual(refused.allowedMentions.roles, []);
    const preview = renderSavedEmbed(tpl, { member: member(MENTION_ALL), guild, channel: thread() });
    assert.deepEqual(preview.allowedMentions, { parse: [] });
    // Publishing reports the typed role as refused for a publisher who may not ping it.
    patch(prisma.savedEmbed, 'findUnique', async ({ where }) => ({ id: where.id, template: tpl, messageId: null, channelId: null }));
    const out = await publishSavedEmbed({ row: { id: 'sav2', name: 'news' }, channel: thread('release-notes'), member: member(NO_MENTION) });
    assert.deepEqual(out.refused, [ROLE]);
  });

  test('the dashboard publish builds its allowlist the same way', () => {
    const resources = {
      guild: { id: GUILD, name: 'Guild', ownerId: OWNER_ID },
      channels: [{ id: THREAD, name: NAME, type: 11, kind: 'text' }],
      roles: [{ id: ROLE, name: 'secret', mentionable: false, everyone: false }],
    };
    const ctx = { userId: PUBLISHER, member: { owner: true, permissions: '0' } }; // may mention every role
    const render = (template) => renderForDiscord(template, resources, { channelId: THREAD, ctx, settings: null, mode: 'publish' });
    const attack = render({ content: 'New in {channel.name}' });
    assert.equal(attack.body.allowed_mentions.roles, undefined, 'no role allowed');
    assert.deepEqual(attack.refused, []);
    assert.match(attack.body.content, /^New in </);
    assert.deepEqual(render({ content: `Ping <@&${ROLE}> in {channel.name}` }).body.allowed_mentions.roles, [ROLE], 'a typed role works for someone who may');
    assert.deepEqual(renderForDiscord({ content: 'x' }, resources, { channelId: THREAD, ctx, settings: null, mode: 'edit' }).body.allowed_mentions, { parse: [] });
  });

  test('free-text built-ins ({channel.name}, {server.name}) cannot carry a live mention', () => {
    const channel = { id: THREAD, name: NAME, type: 11 };
    const text = renderPlaceholders('{channel.name} | {server.name} | {server} | {channel}', { channel, guild: { id: GUILD, name: '<@&1234567890123456> <#42>' } });
    assert.deepEqual(extractRoleMentionIds(text), []);
    assert.ok(!/<@&\d/.test(text) && !/<#4/.test(text.split(' | ')[1]));
    assert.match(text, /<#955060000000000005>$/, 'the real {channel} mention is untouched');
    assert.equal(renderPlaceholders('{channel.name}', { channel: { id: THREAD, name: 'release-notes' } }), 'release-notes');
  });
});
