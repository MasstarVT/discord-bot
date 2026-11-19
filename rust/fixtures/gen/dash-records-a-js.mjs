// The JS side of dash-records-a's cross-language tests: runs the JS dashboard stores (src/dashboard/collections/*.store.js)
// against the test servers (TEST_DATABASE_URL, TEST_REDIS_URL) with a fake Discord REST client and a fake task runner,
// and prints one `RESULT <json>` line. The Rust tests run the same scenario through the Rust stores and compare.
//
//   node rust/fixtures/gen/dash-records-a-js.mjs run <spec.json>    the ops of a scenario (see below)
//   node rust/fixtures/gen/dash-records-a-js.mjs seed <spec.json>   JS-service seeds for each guild of spec.guilds
//   node rust/fixtures/gen/dash-records-a-js.mjs read <spec.json>   list + get of every listed item, per guild
//
// A run spec: { guild, other, seeds, discord, tasks, actor, ops }. Strings in ops may hold $G (guild), $O (other),
// $S<n> (the n-th seed id) and $ID<n> (the id of the item op n answered). Ops:
//   { op: 'list', store, guild?, query?, cursor? } | { op: 'get', store, id } | { op: 'create', store, data, ctx? }
//   { op: 'update', store, id, data, ctx? } | { op: 'remove', store, id, ctx? } | { op: 'action', store, id, action, data, ctx? }
//   { op: 'sql', sql, params? }   (setup; answers the rows) | { op: 'cache', key } | { op: 'job', key }
//   { op: 'fail', method, error } (the fake Discord's next call of method fails) | { op: 'answer', kind, result }
//   { op: 'clock', ms } (both sides: nothing; kept for symmetry)
// ctx: { discord: false } (no REST client), { tasks: false } (no task runner), { member: {...} } (another member),
// { userId } (another actor).
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
process.env.REDIS_URL = process.env.TEST_REDIS_URL;

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

const [cmd, file] = process.argv.slice(2);
const spec = JSON.parse(readFileSync(file, 'utf8'));

const quiet = { info() {}, warn() {}, error() {}, debug() {} };
const storeCache = new Map();
async function storeOf(key) {
  if (!storeCache.has(key)) storeCache.set(key, (await imp('src', 'dashboard', 'collections', `${key}.store.js`)).store);
  return storeCache.get(key);
}

/** The same fake as dash_collections::testing::FakeDiscord. */
function fakeDiscord(cfg, guilds) {
  const calls = [];
  const fail = new Map();
  let next = 1n;
  const users = new Map((cfg.users ?? []).map((u) => [u.id, u]));
  const resources = new Map();
  if (cfg.resources) {
    for (const g of guilds) resources.set(g, { ...cfg.resources, guild: { ...cfg.resources.guild, id: g } });
  }
  const record = (method, args) => {
    calls.push({ method, args });
    const err = fail.get(method);
    if (err) {
      fail.delete(method);
      throw err;
    }
  };
  const apiError = (e) => Object.assign(new Error(e.message), {
    name: 'DiscordApiError', status: e.status ?? 0, code: e.code, ...(e.retryAfter !== undefined ? { retryAfter: e.retryAfter } : {}),
    ...(Number.isInteger(e.discordCode) ? { discordCode: e.discordCode } : {}),
  });
  return {
    calls,
    failNext: (method, e) => fail.set(method, apiError(e)),
    client: {
      async getGuildResources(guildId, opts = {}) {
        record('getGuildResources', { guildId, fresh: opts?.fresh === true });
        const r = resources.get(guildId);
        if (!r) throw apiError({ message: 'Unknown Guild', status: 404, code: 'not_found', discordCode: 10004 });
        return r;
      },
      async getMember(guildId, userId) {
        record('getMember', { guildId, userId });
        const roles = cfg.members?.[userId];
        return roles ? { roles } : null;
      },
      async searchMembers(guildId, query) {
        record('searchMembers', { guildId, query });
        const q = query.toLowerCase();
        return [...users.values()]
          .filter((u) => u.id === query || u.username.toLowerCase().startsWith(q) || (u.nick != null && u.nick.toLowerCase().startsWith(q)))
          .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
          .slice(0, 25);
      },
      async getUsers(ids) {
        record('getUsers', { ids });
        return new Map(ids.slice(0, 100).map((id) => [id, users.get(id) ?? null]));
      },
      async sendMessage(channelId, payload) {
        record('sendMessage', { channelId, payload });
        return { id: String(900000000000000000n + next++), channelId };
      },
      async editMessage(channelId, messageId, payload) {
        record('editMessage', { channelId, messageId, payload });
        return { id: messageId, channelId };
      },
      async deleteMessage(channelId, messageId) {
        record('deleteMessage', { channelId, messageId });
      },
      async createInvite(channelId, reason) {
        record('createInvite', { channelId, reason });
        return { code: 'testcode', url: 'https://discord.gg/testcode', expiresAt: null };
      },
    },
  };
}

/** Replaces $G, $O, $S<n> and $ID<n> in every string. */
function subst(value, vars) {
  if (typeof value === 'string') {
    return value.replace(/\$(ID\d+|S\d+|C\d+|G|O)/g, (m, k) => (vars[k] !== undefined ? vars[k] : m));
  }
  if (Array.isArray(value)) return value.map((v) => subst(v, vars));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, subst(v, vars)]));
  return value;
}

const errorJson = (err) => {
  if (err?.name === 'RecordError') {
    return { code: err.code, message: err.message, ...(err.path ? { path: err.path } : {}), ...(err.saved ? { saved: true } : {}) };
  }
  if (err?.name === 'DiscordApiError') return { discord: err.code, message: err.message };
  if (err?.name === 'FieldError') return { field: err.path, message: err.message };
  return { internal: String(err?.message ?? err) };
};

const plain = (v) => JSON.parse(JSON.stringify(v ?? null));
const idOf = (v) => (v && typeof v === 'object' ? (typeof v.id === 'string' ? v.id : (typeof v.item?.id === 'string' ? v.item.id : undefined)) : undefined);

async function jobOf(key) {
  const row = await prisma.scheduledJob.findUnique({ where: { dedupeKey: key } });
  return row ? { type: row.type, guildId: row.guildId, payload: row.payload, maxAttempts: row.maxAttempts, runInMs: row.runAt.getTime() - Date.now() } : null;
}

async function run() {
  const vars = { G: spec.guild, O: spec.other ?? '' };
  (spec.seeds ?? []).forEach((id, i) => { vars[`S${i}`] = id; });
  const fake = spec.discord ? fakeDiscord(spec.discord, [spec.guild, spec.other].filter(Boolean)) : null;
  const tasks = { ...(spec.tasks ?? {}) };
  const taskCalls = [];
  const actor = spec.actor ?? { userId: '700000000000000001', user: null, member: { owner: true, permissions: '0' } };
  const runTask = (guildId, userId) => async (kind, payload, opts = {}) => {
    taskCalls.push({ guildId, kind, payload: plain(payload), actorId: userId, waitMs: opts.waitMs ?? null });
    const r = tasks[kind];
    if (r?.throw) throw new Error(r.throw);
    return r ? subst(r, vars) : { status: 'done', message: 'Done.', data: null };
  };
  // The feeds' HTTP client: one canned answer for every URL (status changeable with the 'fetch' op).
  const fetchState = { status: 200, body: '', contentType: 'application/rss+xml', calls: 0, ...(spec.fetch ?? {}) };
  if (spec.fetch) {
    const S = await imp('src', 'services', 'feedService.js');
    S.__setFetchImpl({
      safeFetch: async (url) => {
        fetchState.calls++;
        const buffer = Buffer.from(fetchState.body);
        return {
          ok: fetchState.status >= 200 && fetchState.status < 300, status: fetchState.status, statusText: '', headers: { 'content-type': fetchState.contentType },
          url, buffer, text: () => buffer.toString('utf8'), json: () => JSON.parse(buffer.toString('utf8')),
        };
      },
      safeFetchJson: async () => ({}),
    });
  }
  const results = [];
  for (let i = 0; i < spec.ops.length; i++) {
    const op = subst(spec.ops[i], vars);
    const guildId = op.guild ?? spec.guild;
    const c = op.ctx ?? {};
    const discord = c.discord === false ? null : fake?.client ?? null;
    const userId = c.userId ?? actor.userId;
    const ctx = {
      guildId, userId, user: c.user !== undefined ? c.user : actor.user ?? null, member: c.member !== undefined ? c.member : actor.member ?? null,
      discord, logger: quiet, ...(c.tasks === false ? {} : { runTask: runTask(guildId, userId) }),
    };
    const read = { guildId, discord, logger: quiet };
    let out;
    try {
      let v;
      if (op.validate) {
        // What the server does before a write: validateRecordInput against the stored item.
        const { validateRecordInput } = await imp('src', 'dashboard', 'collections', 'index.js');
        const mode = op.op === 'action' ? 'action' : op.op;
        const current = mode === 'create' ? undefined : (await (await storeOf(op.store)).get(guildId, op.id)) ?? undefined;
        op.data = validateRecordInput(op.store, op.data ?? {}, { mode, actionId: op.action, current }).data;
      }
      if (op.op === 'list') {
        const store = await storeOf(op.store);
        v = await store.list(guildId, { query: op.query ?? '', cursor: op.cursor ?? '', discord, logger: quiet }, read);
        v = Array.isArray(v) ? { items: v } : v;
      } else if (op.op === 'get') v = await (await storeOf(op.store)).get(guildId, op.id, read);
      else if (op.op === 'create') v = await (await storeOf(op.store)).create(guildId, op.data, ctx);
      else if (op.op === 'update') v = await (await storeOf(op.store)).update(guildId, op.id, op.data, ctx);
      else if (op.op === 'remove') v = (await (await storeOf(op.store)).remove(guildId, op.id, ctx)) ?? null;
      else if (op.op === 'action') {
        const store = await storeOf(op.store);
        v = await store.actions[op.action](guildId, op.id, op.data ?? {}, ctx);
        v = { item: v?.item ?? null, message: typeof v?.message === 'string' ? v.message : null };
      } else if (op.op === 'sql') { await prisma.$executeRawUnsafe(op.sql); v = null; }
      else if (op.op === 'query') {
        const [row] = await prisma.$queryRawUnsafe(`SELECT COALESCE(json_agg(t), '[]'::json)::text AS j FROM (${op.sql}) t`);
        v = JSON.parse(row.j);
      } else if (op.op === 'settings') {
        const settingsSvc = await imp('src', 'services', 'settingsService.js');
        await settingsSvc.updateSettings(guildId, op.data);
        v = null;
      } else if (op.op === 'cache') v = await cache.get(op.key);
      else if (op.op === 'cacheSet') v = await cache.set(op.key, op.value, op.ttl);
      else if (op.op === 'ttl') v = await redis.ttl(op.key);
      else if (op.op === 'job') v = await jobOf(op.key);
      else if (op.op === 'fail') v = fake.failNext(op.method, op.error) && null;
      else if (op.op === 'fetch') { fetchState.status = op.status; v = null; } else if (op.op === 'fetched') v = fetchState.calls;
      else if (op.op === 'answer') v = (tasks[op.kind] = op.result) && null;
      else throw new Error(`unknown op ${op.op}`);
      if (op.op === 'create' || op.op === 'update') {
        // withMessage answers: { item, message }
        const sym = Object.getOwnPropertySymbols(v ?? {}).find((s) => String(s).includes('recordResult'));
        v = sym ? { item: v.item, message: v.message } : { item: v, message: null };
      }
      out = { ok: plain(v) };
      const id = idOf(op.op === 'create' || op.op === 'update' ? v.item : v);
      if (id !== undefined) vars[`ID${i}`] = id;
      if (typeof v?.nextCursor === 'string') vars[`C${i}`] = v.nextCursor;
    } catch (err) {
      out = { err: errorJson(err) };
    }
    results.push(out);
  }
  return { results, tasks: taskCalls, discord: fake?.calls ?? [] };
}

async function seed() {
  const out = {};
  const settingsSvc = await imp('src', 'services', 'settingsService.js');
  for (const guildId of spec.guilds) {
    const ids = [];
    await settingsSvc.ensureGuildSettings(guildId);
    for (const s0 of spec.steps ?? []) {
      const s = subst(s0, { G: guildId, ...Object.fromEntries(ids.map((x, k) => [`S${k}`, x ?? ''])) });
      let id = null;
      if (s.fn === 'settings') {
        await settingsSvc.updateSettings(guildId, s.data);
      } else if (s.fn === 'setXp') {
        const xp = await imp('src', 'services', 'xpService.js');
        await xp.setXp({ guildId, userId: s.userId, xp: s.xp });
        id = s.userId;
      } else if (s.fn === 'applyXpDelta') {
        const xp = await imp('src', 'services', 'xpService.js');
        await xp.applyXpDelta({ guildId, userId: s.userId, delta: s.delta, messages: s.messages ?? 0, voiceMinutes: s.voiceMinutes ?? 0, touchLastXp: true });
        id = s.userId;
      } else if (s.fn === 'credit') {
        const eco = await imp('src', 'services', 'economyService.js');
        await eco.credit(guildId, s.userId, s.amount, await settingsSvc.getSettings(guildId, { fresh: true }));
        id = s.userId;
      } else if (s.fn === 'setBalance') {
        const eco = await imp('src', 'services', 'economyService.js');
        await eco.setBalance(guildId, s.userId, s.balance, await settingsSvc.getSettings(guildId, { fresh: true }));
        id = s.userId;
      } else if (s.fn === 'recordInfraction') {
        const mod = await imp('src', 'services', 'moderationService.js');
        const row = await mod.recordInfraction({ ...s.data, guildId, expiresAt: s.expiresInMs ? new Date(Date.now() + s.expiresInMs) : null });
        id = String(row.caseId);
      } else if (s.fn === 'createPollRow') {
        const polls = await imp('src', 'services', 'pollService.js');
        const r = await polls.createPollRow(guildId, s.pending, s.input);
        if (!r.ok) throw new Error(`createPollRow: ${r.code}`);
        if (s.update) await prisma.poll.update({ where: { id: r.poll.id }, data: reviveDates(s.update) });
        id = r.poll.id;
      } else if (s.fn === 'applyVote') {
        const polls = await imp('src', 'services', 'pollService.js');
        await polls.applyVote({ pollId: ids[s.poll], userId: s.userId, maxChoices: s.maxChoices, idx: s.idx ?? null, set: s.set ?? null });
      } else if (s.fn === 'create') {
        const row = await prisma[s.model].create({ data: reviveDates({ ...s.data, ...(s.noGuild ? {} : { guildId }) }) });
        id = row.id ?? null;
      } else if (s.fn === 'createMany') {
        await prisma[s.model].createMany({ data: s.data.map((d) => reviveDates({ ...d, guildId })) });
      } else {
        throw new Error(`unknown seed ${s.fn}`);
      }
      ids.push(id);
    }
    out[guildId] = ids;
  }
  return out;
}

/** `{ "$date": ms-from-now }` and `{ "$at": iso }` values become Dates. */
function reviveDates(v) {
  if (Array.isArray(v)) return v.map(reviveDates);
  if (v && typeof v === 'object') {
    if (Object.hasOwn(v, '$date')) return new Date(Date.now() + v.$date);
    if (Object.hasOwn(v, '$at')) return new Date(v.$at);
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, reviveDates(x)]));
  }
  return v;
}

async function readBack() {
  const out = {};
  const fake = spec.discord ? fakeDiscord(spec.discord, spec.guilds) : null;
  const store = await storeOf(spec.store);
  for (const guildId of spec.guilds) {
    const read = { guildId, discord: fake?.client ?? null, logger: quiet };
    let list = await store.list(guildId, { query: '', cursor: '', discord: read.discord, logger: quiet }, read);
    list = Array.isArray(list) ? { items: list } : list;
    const gets = [];
    for (const item of list.items) gets.push(await store.get(guildId, item.id, read));
    out[guildId] = plain({ list, gets });
  }
  return out;
}

try {
  if (cmd === 'run') answer(await run());
  else if (cmd === 'seed') answer(await seed());
  else if (cmd === 'read') answer(await readBack());
  else throw new Error(`unknown command ${cmd}`);
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
}
redis.disconnect();
await prisma.$disconnect();
process.exit(0);
