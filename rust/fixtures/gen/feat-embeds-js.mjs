// The JS side of feat-embeds' cross-language tests: the JS bot's own embedService against the test database and
// Redis (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-embeds-js.mjs create <guildId> <name> <json template> <createdBy>   createSavedEmbed
//   node rust/fixtures/gen/feat-embeds-js.mjs publish <id> <channelId> <messageId>                setPublished
//   node rust/fixtures/gen/feat-embeds-js.mjs mutate <guildId> <name> <json field>                mutateSavedEmbed(addField)
//   node rust/fixtures/gen/feat-embeds-js.mjs get <guildId> <name>                                getSavedEmbed (+ normalizeTemplate)
//   node rust/fixtures/gen/feat-embeds-js.mjs template <guildId> <name>                           getSavedEmbedTemplate
//   node rust/fixtures/gen/feat-embeds-js.mjs cache <guildId> <name>                              the raw cache value (cache.get)
//   node rust/fixtures/gen/feat-embeds-js.mjs search <guildId> <query> <take>                     searchSavedEmbeds
//   node rust/fixtures/gen/feat-embeds-js.mjs list <guildId>                                      listSavedEmbeds
//   node rust/fixtures/gen/feat-embeds-js.mjs count <guildId>                                     countSavedEmbeds
//   node rust/fixtures/gen/feat-embeds-js.mjs ondelete <guildId> <messageId>                      onEmbedMessageDelete
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

const svc = await imp('src', 'services', 'embedService.js');
const { normalizeTemplate } = await imp('src', 'utils', 'customEmbed.js');
const { cache, default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');

const [cmd, ...args] = process.argv.slice(2);
try {
  // Give the Redis client a moment to connect, so the cache is really used.
  for (let i = 0; i < 50 && redis.status !== 'ready'; i++) await new Promise((r) => setTimeout(r, 50));
  if (cmd === 'create') {
    const row = await svc.createSavedEmbed(args[0], args[1], JSON.parse(args[2]), args[3]);
    answer({ row, template: normalizeTemplate(row.template) });
  } else if (cmd === 'publish') {
    await svc.setPublished(args[0], { channelId: args[1], messageId: args[2] });
    answer({ ok: true });
  } else if (cmd === 'mutate') {
    const row = await svc.mutateSavedEmbed(args[0], args[1], (t) => svc.addField(t, JSON.parse(args[2])));
    answer({ row, template: normalizeTemplate(row.template) });
  } else if (cmd === 'get') {
    const row = await svc.getSavedEmbed(args[0], args[1]);
    answer(row ? { row, template: normalizeTemplate(row.template) } : null);
  } else if (cmd === 'template') {
    answer(await svc.getSavedEmbedTemplate(args[0], args[1]));
  } else if (cmd === 'cache') {
    answer(await cache.get(svc.embedCacheKey(args[0], args[1])));
  } else if (cmd === 'search') {
    answer(await svc.searchSavedEmbeds(args[0], args[1], Number(args[2])));
  } else if (cmd === 'list') {
    answer(await svc.listSavedEmbeds(args[0]));
  } else if (cmd === 'count') {
    answer(await svc.countSavedEmbeds(args[0]));
  } else if (cmd === 'ondelete') {
    await svc.onEmbedMessageDelete({ guildId: args[0], message: { id: args[1] } });
    answer({ ok: true });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err), code: err?.code ?? null });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
