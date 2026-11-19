// The JS side of feat-general's cross-language tests: the JS bot's own services against the test database
// (TEST_DATABASE_URL) and Redis (TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-general-js.mjs bot-record <guildId> <inviterId> <inviterName>
//       botJoinService.recordBotJoin on startup, for a guild whose audit log names the inviter
//   node rust/fixtures/gen/feat-general-js.mjs bot-list <json array of guild ids>      botJoinService.listBotJoins
//   node rust/fixtures/gen/feat-general-js.mjs settings-get <guildId>                  settingsService.getSettings (fresh)
//   node rust/fixtures/gen/feat-general-js.mjs settings-update <guildId> <json data>   settingsService.updateSettings
//   node rust/fixtures/gen/feat-general-js.mjs tags <guildId> <json tags> <json viewer role ids or null>
//       writes the CustomTag rows (there is no tag-creating service; /tag create writes them with Prisma the same
//       way), then help.customCommandGroup with the stored settings
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { Collection, PermissionFlagsBits } = await import('discord.js');
const svc = await imp('src', 'services', 'botJoinService.js');
const settings = await imp('src', 'services', 'settingsService.js');
const help = await imp('src', 'commands', 'utility', 'help.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const BOT = '100000000000000002';
const pick = (row, keys) => Object.fromEntries(keys.map((k) => [k, row?.[k] ?? null]));

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'bot-record') {
    const [guildId, inviterId, inviterName] = args;
    const guild = {
      id: guildId,
      joinedAt: new Date('2026-10-01T12:00:00.000Z'),
      members: { me: { permissions: { has: (bit) => bit === PermissionFlagsBits.ViewAuditLog } } },
      fetchAuditLogs: async () => ({ entries: new Collection([['1', { targetId: BOT, executorId: inviterId, executor: { id: inviterId, tag: inviterName } }]]) }),
      fetchIntegrations: async () => new Collection(),
    };
    await svc.recordBotJoin({ client: { user: { id: BOT }, application: { id: BOT } }, guild, reason: 'startup' });
    answer(Object.fromEntries(await svc.listBotJoins([guildId])));
  } else if (cmd === 'bot-list') {
    answer(Object.fromEntries(await svc.listBotJoins(JSON.parse(args[0]))));
  } else if (cmd === 'settings-get') {
    const row = await settings.getSettings(args[0], { fresh: true });
    answer(pick(row, ['timezone', 'timezoneRoles', 'embedColor', 'customPrefix', 'urbanNsfwOnly', 'xpEnabled', 'invitesEnabled', 'pollsEnabled']));
  } else if (cmd === 'settings-update') {
    const row = await settings.updateSettings(args[0], JSON.parse(args[1]));
    answer(pick(row, ['timezone', 'timezoneRoles', 'embedColor', 'customPrefix', 'urbanNsfwOnly']));
  } else if (cmd === 'tags') {
    const [guildId, tagsJson, viewerJson] = args;
    await settings.ensureGuildSettings(guildId);
    for (const t of JSON.parse(tagsJson)) {
      await prisma.customTag.create({ data: { guildId, content: 'x', createdBy: '1', ...t } });
    }
    const row = await settings.getSettings(guildId, { fresh: true });
    const ids = JSON.parse(viewerJson);
    const viewer = ids ? { roles: { cache: new Map(ids.map((id) => [id, { id }])) } } : null;
    const group = await help.customCommandGroup(guildId, row, viewer);
    answer(group);
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
