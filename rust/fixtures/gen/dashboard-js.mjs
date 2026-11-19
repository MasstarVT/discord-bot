// The JS side of bins/dashboard's cross-language tests: the JS dashboard's settings bridge (src/dashboard/settingsBridge.js)
// and records bridge (src/dashboard/recordsBridge.js) against the test database and Redis (TEST_DATABASE_URL /
// TEST_REDIS_URL), with a recording Discord client and task runner. Prints one `RESULT <json>` line.
// The task protocol (src/dashboard/tasks.js and the bot's runDashboardTask) is driven by feat-dashboard-tasks-js.mjs.
//
//   settings-read <guildId>                                   readGuildSettings (the normalised row)
//   settings-write <guildId> <json body> <json resources|null> validateSettingsChanges(body), then writeSettings with a
//                                                             recording Discord client serving <resources>
//   records <json ops> <json resources|null>                  createRecordsBridge(...) runs the ops in order:
//       { op: 'list'|'get'|'create'|'update'|'remove'|'action', guild, key, id?, data?, actionId?, query?, cursor?,
//         actor? }; data of create/update goes through validateRecordInput first (an action's data is passed as
//       given, so the bridge's own refusals show); an id "$<n>" is the id of the item op <n> answered
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
const [cmd, ...args] = process.argv.slice(2);
console.error = console.warn = console.log = console.debug = console.info = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');
const quiet = { info() {}, warn() {}, error() {}, debug() {}, success() {} };

function fakeDiscord(resources) {
  const calls = [];
  let next = 0;
  const msg = (channelId) => ({ id: `13000000000000${String(++next).padStart(5, '0')}`, channelId });
  return {
    calls,
    client: {
      async getGuildResources(guildId, opts = {}) {
        calls.push({ method: 'getGuildResources', args: [guildId, opts] });
        if (!resources) throw Object.assign(new Error('no resources'), { status: 0, code: 'unavailable' });
        return resources;
      },
      async getMember(guildId, userId) { calls.push({ method: 'getMember', args: [guildId, userId] }); return null; },
      async searchMembers(guildId, query) { calls.push({ method: 'searchMembers', args: [guildId, query] }); return []; },
      async getUsers(ids) { calls.push({ method: 'getUsers', args: [ids] }); return new Map(ids.map((id) => [id, null])); },
      async sendMessage(channelId, payload) { calls.push({ method: 'sendMessage', args: [channelId, payload] }); return msg(channelId); },
      async editMessage(channelId, messageId, payload) {
        calls.push({ method: 'editMessage', args: [channelId, messageId, payload] });
        return { id: messageId, channelId };
      },
      async deleteMessage(channelId, messageId) { calls.push({ method: 'deleteMessage', args: [channelId, messageId] }); },
      async createInvite(channelId, opts) { calls.push({ method: 'createInvite', args: [channelId, opts] }); return { code: 'abc', url: 'https://discord.gg/abc' }; },
    },
  };
}

const errorJson = (err) => ({ name: err?.name ?? 'Error', code: err?.code ?? null, message: err?.message ?? String(err), path: err?.path ?? null, saved: err?.saved ?? false });

try {
  if (cmd === 'settings-read') {
    const settings = await imp('src', 'dashboard', 'settingsBridge.js');
    answer(await settings.readGuildSettings(args[0]));
  } else if (cmd === 'settings-write') {
    const [guildId, rawBody, rawResources] = args;
    const settings = await imp('src', 'dashboard', 'settingsBridge.js');
    const { validateSettingsChanges } = await imp('src', 'dashboard', 'fields.js');
    const valid = validateSettingsChanges(JSON.parse(rawBody));
    if (!valid.ok) {
      answer({ invalid: { message: valid.message, path: valid.path ?? null } });
    } else {
      const d = fakeDiscord(JSON.parse(rawResources));
      try {
        const out = await settings.writeSettings(guildId, valid.changes, { discord: d.client, logger: quiet, userId: '600000000000000001' });
        answer({ changes: valid.changes, row: out.row, notices: out.notices, calls: d.calls });
      } catch (err) {
        answer({ changes: valid.changes, error: errorJson(err), calls: d.calls });
      }
    }
  } else if (cmd === 'records') {
    const [rawOps, rawResources] = args;
    const { createRecordsBridge } = await imp('src', 'dashboard', 'recordsBridge.js');
    const { validateRecordInput } = await imp('src', 'dashboard', 'collections', 'index.js');
    const d = fakeDiscord(JSON.parse(rawResources));
    const taskCalls = [];
    const tasks = { async run(guildId, kind, payload, opts) { taskCalls.push({ guildId, kind, payload, opts }); return { status: 'done', message: 'Done.', data: null }; } };
    const bridge = createRecordsBridge({ discord: d.client, logger: quiet, tasks });
    const out = [];
    const idOf = (id) => (typeof id === 'string' && id.startsWith('$') ? out[Number(id.slice(1))]?.ok?.item?.id ?? out[Number(id.slice(1))]?.ok?.id : id);
    for (const o of JSON.parse(rawOps)) {
      const actor = o.actor ?? { userId: '600000000000000001', user: null, member: { owner: true, permissions: '8' } };
      try {
        const id = idOf(o.id);
        let value;
        if (o.op === 'list') value = await bridge.list(o.guild, o.key, { query: o.query ?? '', cursor: o.cursor ?? '' });
        else if (o.op === 'get') value = await bridge.get(o.guild, o.key, id);
        else if (o.op === 'remove') value = await bridge.remove(o.guild, o.key, id, actor) ?? null;
        else if (o.op === 'action') value = await bridge.action(o.guild, o.key, id, o.actionId, o.data ?? {}, actor);
        else {
          const { data } = validateRecordInput(o.key, o.data ?? {}, { mode: o.op, current: null });
          if (o.op === 'create') value = await bridge.create(o.guild, o.key, data, actor);
          else value = await bridge.update(o.guild, o.key, id, data, actor);
        }
        out.push({ ok: value === undefined ? null : value });
      } catch (err) {
        out.push({ error: errorJson(err) });
      }
    }
    answer({ results: out, discord: d.calls, tasks: taskCalls });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
