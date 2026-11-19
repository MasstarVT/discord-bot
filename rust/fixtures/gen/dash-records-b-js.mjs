// The JS side of dash-records-b's cross-language tests: runs the JS dashboard record stores
// (src/dashboard/collections/*.store.js) against the test servers (TEST_DATABASE_URL, TEST_REDIS_URL) and prints one
// `RESULT <json>` line. The Rust test runs the same steps through the Rust stores on another guild and compares.
//
//   node rust/fixtures/gen/dash-records-b-js.mjs <spec.json>
//
// spec: { guild, ctxs: { <name>: Ctx }, steps: [Step] }. "$G" anywhere in a string of the spec is replaced by `guild`,
// and "@id<n>" (n = a step index) by the `id` of the item that step answered.
//   Ctx:  { userId, user?, member?, resources?: object | null, discordFail?: { <method>: { message, status, code,
//           discordCode? } }, tasks?: null | { <kind>: { status, message, data } } }
//         (`resources` absent: no ctx.discord; `tasks` absent: no ctx.runTask; a kind without an answer is done)
//   Step: { store, method: list|get|create|update|remove|action, id?, data?, action?, ctx?: <name>,
//           validate?: true (validateRecordInput first: create, update against the current item, action) }
//       | { op: 'sql', sql, params? }       raw statement ($1.. binds are strings)
//       | { op: 'query', sql, params? }     rows as JSON (row_to_json)
//       | { op: 'cache', key }              the Redis value (parsed when it is JSON)
//       | { op: 'cacheset', key, value, ttl? } writes a JSON value
//       | { op: 'ttl', key }                the Redis TTL in seconds
//       | { op: 'settings', patch }         settingsService.updateSettings
// Each step answers { out } or { error: { code, message, path?, saved? } } or { internal }, plus the task runs
// ({ kind, payload }) and the Discord calls ({ method, args }) it made.
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production'; // no debug lines on stdout
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL;
process.env.REDIS_URL = process.env.TEST_REDIS_URL;

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));
const { RecordError, unwrapResult } = await imp('src', 'dashboard', 'collections', 'errors.js');
const { validateRecordInput } = await imp('src', 'dashboard', 'collections', 'index.js');

const spec = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const G = spec.guild;
const ids = [];

/** "$G" -> the guild and "@id<n>" -> the id step n answered, anywhere in a string of the spec. */
function fill(v) {
  if (typeof v === 'string') return v.replaceAll('$G', G).replace(/@id(\d+)/g, (m, n) => ids[Number(n)] ?? m);
  if (Array.isArray(v)) return v.map(fill);
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, fill(x)]));
  return v;
}

class FakeDiscordError extends Error {
  constructor(message, { status = 0, code, discordCode } = {}) {
    super(message);
    this.name = 'DiscordApiError';
    this.status = status;
    this.code = code;
    if (Number.isInteger(discordCode)) this.discordCode = discordCode;
  }
}

function ctxOf(name, log) {
  const c = fill(spec.ctxs?.[name ?? 'default'] ?? {});
  const ctx = { guildId: G, userId: c.userId ?? '0', user: c.user ?? null, member: c.member ?? null, logger: undefined };
  let n = 0n;
  const failOf = (method) => {
    const f = c.discordFail?.[method];
    if (f) throw new FakeDiscordError(f.message, f);
  };
  if (c.resources !== undefined) {
    ctx.discord = {
      async getGuildResources(guildId, opts = {}) {
        log.discord.push({ method: 'getGuildResources', args: { guildId, fresh: opts?.fresh === true } });
        failOf('getGuildResources');
        if (!c.resources) throw new FakeDiscordError('Unknown Guild', { status: 404, code: 'not_found', discordCode: 10004 });
        return structuredClone(c.resources);
      },
      async sendMessage(channelId, payload) {
        log.discord.push({ method: 'sendMessage', args: { channelId, payload } });
        failOf('sendMessage');
        n += 1n;
        return { id: String(900000000000000000n + n), channelId };
      },
      async editMessage(channelId, messageId, payload) {
        log.discord.push({ method: 'editMessage', args: { channelId, messageId, payload } });
        failOf('editMessage');
        return { id: messageId, channelId };
      },
      async deleteMessage(channelId, messageId) {
        log.discord.push({ method: 'deleteMessage', args: { channelId, messageId } });
        failOf('deleteMessage');
      },
    };
  }
  if (c.tasks !== undefined && c.tasks !== null) {
    ctx.runTask = async (kind, payload) => {
      log.tasks.push({ kind, payload: JSON.parse(JSON.stringify(payload)) });
      return c.tasks[kind] ?? { status: 'done', message: 'Done.', data: null };
    };
  }
  return ctx;
}

const storeCache = {};
async function storeOf(key) {
  storeCache[key] ??= (await imp('src', 'dashboard', 'collections', `${key}.store.js`)).store;
  return storeCache[key];
}

const errorOf = (err) => {
  if (err instanceof RecordError) {
    const e = { code: err.code, message: err.message };
    if (err.path) e.path = err.path;
    if (err.saved) e.saved = true;
    return { error: e };
  }
  if (err?.name === 'FieldError') return { field: { path: err.path, message: err.message } };
  return { internal: String(err?.message ?? err) };
};

async function runStep(step, log) {
  if (step.op === 'sql') return { out: await prisma.$executeRawUnsafe(step.sql, ...(step.params ?? [])) };
  if (step.op === 'query') {
    const rows = await prisma.$queryRawUnsafe(`SELECT row_to_json(t) AS r FROM (${step.sql}) t`, ...(step.params ?? []));
    return { out: rows.map((x) => x.r) };
  }
  if (step.op === 'cache') {
    const raw = await redis.get(step.key);
    try {
      return { out: raw === null ? null : JSON.parse(raw) };
    } catch {
      return { out: raw };
    }
  }
  if (step.op === 'cacheset') return { out: (await redis.set(step.key, JSON.stringify(step.value), 'EX', step.ttl ?? 300)) === 'OK' };
  if (step.op === 'settings') {
    const { updateSettings } = await imp('src', 'services', 'settingsService.js');
    await updateSettings(G, step.patch);
    return { out: null };
  }
  if (step.op === 'ttl') return { out: await redis.ttl(step.key) };
  const store = await storeOf(step.store);
  const ctx = ctxOf(step.ctx, log);
  let data = step.data ?? {};
  if (step.validate) {
    if (step.method === 'create') data = validateRecordInput(step.store, data, { mode: 'create' }).data;
    else if (step.method === 'update') {
      const current = await store.get(G, step.id, ctx);
      data = validateRecordInput(step.store, data, { mode: 'update', current }).data;
    } else if (step.method === 'action') {
      const current = await store.get(G, step.id, ctx);
      data = validateRecordInput(step.store, data, { mode: 'action', actionId: step.action, current }).data;
    }
  }
  switch (step.method) {
    case 'list': return { out: await store.list(G, { query: '', cursor: '' }, ctx) };
    case 'get': return { out: await store.get(G, step.id, ctx) };
    case 'create': return { saved: unwrapResult(await store.create(G, data, ctx)) };
    case 'update': return { saved: unwrapResult(await store.update(G, step.id, data, ctx)) };
    case 'remove': await store.remove(G, step.id, ctx); return { out: null };
    case 'action': {
      const fn = store.actions?.[step.action];
      if (typeof fn !== 'function') throw new RecordError('not_found', 'Unknown action.');
      const r = await fn(G, step.id, data, ctx);
      return { action: { item: r?.item ?? null, message: typeof r?.message === 'string' ? r.message : null } };
    }
    default: throw new Error(`unknown method ${step.method}`);
  }
}

const results = [];
try {
  for (const raw of spec.steps) {
    const step = fill(raw);
    const log = { tasks: [], discord: [] };
    let r;
    try {
      r = await runStep(step, log);
    } catch (err) {
      r = errorOf(err);
    }
    const item = r.saved?.item ?? r.out;
    ids.push(item && typeof item === 'object' && !Array.isArray(item) ? item.id : undefined);
    results.push({ ...r, tasks: log.tasks, discord: log.discord });
  }
  answer({ results });
} catch (err) {
  answer({ fatal: String(err?.stack ?? err) });
}
redis.disconnect();
await prisma.$disconnect();
process.exit(0);
