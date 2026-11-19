// The JS side of feat-reactionroles' cross-language tests: the JS bot's own reactionRoleService against the test
// database and Redis (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-reactionroles-js.mjs seed <guildId>            createPanel + addItem + updatePanel + getPanelIndex
//   node rust/fixtures/gen/feat-reactionroles-js.mjs get <guildId> <panelId>   getPanel
//   node rust/fixtures/gen/feat-reactionroles-js.mjs list <guildId>            listPanels
//   node rust/fixtures/gen/feat-reactionroles-js.mjs index <guildId>           getPanelIndex (fresh process: Redis, then Postgres)
//   node rust/fixtures/gen/feat-reactionroles-js.mjs search <guildId> <q>      searchPanels
//   node rust/fixtures/gen/feat-reactionroles-js.mjs items <guildId> <panelId> <q> <roleIds json>   searchItems
//   node rust/fixtures/gen/feat-reactionroles-js.mjs rate <guildId> <userId> <n>  n reactions through processReaction
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { randomUUID } from 'node:crypto';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const rr = await imp('src', 'services', 'reactionRoleService.js');
const { ensureGuildSettings } = await imp('src', 'services', 'settingsService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const [cmd, ...args] = process.argv.slice(2);
// The cache fails open while the client connects: wait for it so a read really reaches Redis.
if (redis.status !== 'ready') await Promise.race([new Promise((r) => redis.once('ready', r)), new Promise((r) => setTimeout(r, 5000))]);
try {
  if (cmd === 'seed') {
    const g = args[0];
    await ensureGuildSettings(g);
    const a = randomUUID();
    await rr.createPanel({
      id: a, guildId: g, channelId: '300000000000000101', messageId: '500000000000000101', title: 'JS buttons', description: 'Pick',
      type: 'NORMAL', layout: 'BUTTONS', maxRoles: 2, plainText: false, messageContent: 'Hello', attached: false, placeholder: null,
      items: [
        rr.draftItem(a, { roleId: '400000000000000101', label: 'One', emoji: '🔔', style: 3 }, 0),
        rr.draftItem(a, { roleId: '400000000000000102', label: 'Two', emoji: '<:pog:123456789012345678>', description: 'second' }, 1),
      ],
    });
    const b = randomUUID();
    await rr.createPanel({
      id: b, guildId: g, channelId: '300000000000000102', messageId: '500000000000000102', title: 'JS reactions', type: 'UNIQUE',
      layout: 'REACTIONS', attached: true, items: [rr.draftItem(b, { roleId: '400000000000000103', label: 'Heart', emoji: '❤️' }, 0)],
    });
    const panelA = await rr.getPanel(g, a);
    await rr.addItem(panelA, { roleId: '400000000000000104', label: 'Three', style: 2 });
    await rr.updatePanel(g, a, { placeholder: 'Choose', messageMissingAt: new Date('2026-03-04T05:06:07.890Z') });
    const index = await rr.getPanelIndex(g);
    answer({
      ids: [a, b],
      panels: [await rr.getPanel(g, a), await rr.getPanel(g, b)],
      list: await rr.listPanels(g),
      index: [...index.values()],
      cacheRaw: await redis.get(rr.indexKey(g)),
    });
  } else if (cmd === 'get') {
    answer(await rr.getPanel(args[0], args[1]));
  } else if (cmd === 'list') {
    answer(await rr.listPanels(args[0]));
  } else if (cmd === 'index') {
    const index = await rr.getPanelIndex(args[0]);
    answer(index ? [...index.values()] : null);
  } else if (cmd === 'search') {
    answer(await rr.searchPanels(args[0], args[1]));
  } else if (cmd === 'items') {
    answer(await rr.searchItems(args[0], args[1], args[2], JSON.parse(args[3])));
  } else if (cmd === 'rate') {
    const [g, u, n] = args;
    rr.__setPanelIndex(g, [{ id: 'p', channelId: '300000000000000301', messageId: '500000000000000301', title: 'T', type: 'NORMAL',
      layout: 'REACTIONS', maxRoles: null, attached: false, items: [{ roleId: '400000000000000301', emoji: '🔔', key: '🔔' }] }]);
    let members = 0;
    for (let i = 0; i < Number(n); i++) {
      const ctx = { guildId: g, message: { id: '500000000000000301' }, user: { id: u }, emojiKey: '🔔', getMember: async () => { members++; return null; } };
      await rr.processReaction(ctx, 'react');
    }
    answer({ members });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
