// The JS side of feat-automations' cross-language tests: the JS bot's own automationService against the test database
// and Redis (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-automations-js.mjs create <guildId> <json data> <createdBy> [selfRefId]   createAutomation
//   node rust/fixtures/gen/feat-automations-js.mjs mutate <guildId> <name> <json data>                   mutateAutomation (data = the columns)
//   node rust/fixtures/gen/feat-automations-js.mjs get <guildId> <name>                                  getAutomationByName
//   node rust/fixtures/gen/feat-automations-js.mjs list <guildId>                                        listAutomations
//   node rust/fixtures/gen/feat-automations-js.mjs buttons <guildId>                                     listAutomationButtons
//   node rust/fixtures/gen/feat-automations-js.mjs search <guildId> <q>                                  searchAutomations
//   node rust/fixtures/gen/feat-automations-js.mjs export <guildId> <name>                               toExportJson (with the guild's names)
//   node rust/fixtures/gen/feat-automations-js.mjs load <guildId>                                        getAutomations (Redis first)
//   node rust/fixtures/gen/feat-automations-js.mjs raw <key>                                             the raw Redis value and TTL
//   node rust/fixtures/gen/feat-automations-js.mjs fire <guildId> <userId> <channelId>                   dispatch MESSAGE_SENT (fake guild): cooldowns
//   node rust/fixtures/gen/feat-automations-js.mjs execute <guildId> <name>                              executeAutomation (fake guild): problems
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value === undefined ? null : value)}\n`);

const svc = await imp('src', 'services', 'automationService.js');
const { cache, default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { Collection } = await import('discord.js');

/** A fake guild, as test/automations.test.js builds it: every permission granted, sends recorded. */
function fakeGuild(id, channelId, userId) {
  const guild = {
    id, available: true, ownerId: '1', name: 'G', memberCount: 1,
    channels: { cache: new Collection() }, roles: { cache: new Collection() },
    members: { cache: new Collection(), me: { id: '900000000000000099', permissions: { has: () => true }, roles: { highest: { position: 50 } } } },
    emojis: { cache: new Collection() }, client: { user: { id: '900000000000000099' } },
  };
  const sent = [];
  const channel = {
    id: channelId, type: 0, name: 'general', parentId: null, guild, sent,
    isTextBased: () => true, isThread: () => false, permissionsFor: () => ({ has: () => true }),
    send: async (p) => { sent.push(p); return { id: 's' }; },
  };
  guild.channels.cache.set(channelId, channel);
  const member = {
    id: userId, guild, displayName: 'm', user: { id: userId, bot: false, username: 'u', displayAvatarURL: () => '' },
    roles: { cache: new Collection(), async add() {}, async remove() {} },
  };
  guild.members.cache.set(userId, member);
  return { guild, channel, member, sent };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  for (let i = 0; i < 50 && redis.status !== 'ready'; i++) await new Promise((r) => setTimeout(r, 50));
  if (cmd === 'create') {
    answer(await svc.createAutomation(args[0], JSON.parse(args[1]), args[2], { selfRefId: args[3] ?? null }));
  } else if (cmd === 'mutate') {
    const data = JSON.parse(args[2]);
    answer((await svc.mutateAutomation(args[0], args[1], () => ({ data }))).row);
  } else if (cmd === 'get') {
    answer(await svc.getAutomationByName(args[0], args[1]));
  } else if (cmd === 'list') {
    answer(await svc.listAutomations(args[0]));
  } else if (cmd === 'buttons') {
    answer(await svc.listAutomationButtons(args[0]));
  } else if (cmd === 'search') {
    answer(await svc.searchAutomations(args[0], args[1]));
  } else if (cmd === 'export') {
    const row = await svc.getAutomationByName(args[0], args[1]);
    const refs = await svc.listAutomationRefs(args[0]);
    answer(svc.toExportJson(row, new Map(refs.map((r) => [r.id, { name: r.name }]))));
  } else if (cmd === 'load') {
    answer(await svc.getAutomations(args[0]));
  } else if (cmd === 'raw') {
    answer({ value: await redis.get(args[0]), ttl: await redis.ttl(args[0]) });
  } else if (cmd === 'fire') {
    const [g, userId, channelId] = args;
    const { guild, channel, member, sent } = fakeGuild(g, channelId, userId);
    const message = {
      id: '700000000000000001', content: 'hello', type: 0, author: member.user, channel, guild, system: false, partial: false, pinned: false,
      member, url: '', createdTimestamp: 0, attachments: new Collection(), reactions: { cache: new Collection() },
    };
    await svc.dispatch('MESSAGE_SENT', {
      guild, guildId: g, client: { user: { id: '900000000000000099' } }, depth: 0, getSettings: async () => null,
      trigger: 'MESSAGE_SENT', member, user: member.user, channel, message,
    }, { awaitRuns: true });
    answer({ sent: sent.map((p) => p.content) });
  } else if (cmd === 'execute') {
    const [g, name] = args;
    const row = await svc.getAutomationByName(g, name);
    const { guild, member, sent } = fakeGuild(g, '100000000000000040', '100000000000000042');
    await svc.executeAutomation(
      { id: row.id, name: row.name, trigger: row.trigger, cooldownSeconds: row.cooldownSeconds, conditions: row.conditions, actions: row.actions },
      { guild, guildId: g, client: { user: { id: '900000000000000099' } }, depth: 0, getSettings: async () => null, trigger: row.trigger, member, user: member.user, channel: null },
    );
    await new Promise((r) => setTimeout(r, 100));
    answer({ sent: sent.length, problem: await cache.get(`automation:problem:${row.id}`) });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err), code: err?.code ?? null });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
