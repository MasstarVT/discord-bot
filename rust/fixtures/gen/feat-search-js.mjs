// The JS side of feat-search's cross-language tests: the JS bot's own afkService and searchService against the test
// servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   afk-set <guildId> <userId> <json { reason, nickChanged, previousNick }>   afkService.setAfk
//   afk-ping <guildId> <userId> <channelId> <messageId> <authorId>            afkService.recordPing
//   afk-load <guildId> <userId>                                              afkService.loadStatus
//   afk-list <guildId>                                                       afkService.listStatuses
//   afk-ids <guildId>                                                        afkService.loadAfkIds (sorted)
//   anilist <query> <allowAdult 0|1> <json canned { status, headers, body }> [userId] [guildId]
//                                                                            searchService.searchAniList (canned safeFetch)
//   urban <term> <json canned body>                                          searchService.defineUrban (canned safeFetchJson)
//   pokemon <slug> <json { routes }>                                         searchService.getPokemon (canned safeFetchJson)
//   index                                                                    searchService.getPokemonIndex (no network)
// Every search command also reports how many requests reached the canned network.
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const afk = await imp('src', 'services', 'afkService.js');
const search = await imp('src', 'services', 'searchService.js');
const { SafeFetchError } = await imp('src', 'utils', 'safeFetch.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

let calls = 0;
const res = ({ status = 200, headers = {}, body = null }) => ({
  ok: status >= 200 && status < 300, status, headers, json() { if (body === null) throw new SyntaxError('not json'); return structuredClone(body); },
});
const outcome = async (p) => {
  try {
    const r = await p;
    return { results: r.results, cached: r.cached, calls };
  } catch (err) {
    return { code: err.code ?? String(err), retryAt: err.retryAt ?? null, calls };
  }
};

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'afk-set') {
    const o = JSON.parse(args[2]);
    answer(await afk.setAfk({ guildId: args[0], userId: args[1], reason: o.reason, nickChanged: o.nickChanged, previousNick: o.previousNick }));
  } else if (cmd === 'afk-ping') {
    await afk.recordPing(args[0], args[1], { channelId: args[2], messageId: args[3], authorId: args[4] });
    answer(await afk.loadStatus(args[0], args[1]));
  } else if (cmd === 'afk-load') {
    answer(await afk.loadStatus(args[0], args[1]));
  } else if (cmd === 'afk-list') {
    answer(await afk.listStatuses(args[0]));
  } else if (cmd === 'afk-ids') {
    answer([...(await afk.loadAfkIds(args[0]))].sort());
  } else if (cmd === 'anilist') {
    const canned = JSON.parse(args[2]);
    search.__setDeps({ safeFetch: async () => { calls += 1; return res(canned); } });
    answer(await outcome(search.searchAniList('ANIME', args[0], { allowAdult: args[1] === '1', userId: args[3] || null, guildId: args[4] || null })));
  } else if (cmd === 'urban') {
    const body = JSON.parse(args[1]);
    search.__setDeps({ safeFetchJson: async () => { calls += 1; return structuredClone(body); } });
    answer(await outcome(search.defineUrban(args[0])));
  } else if (cmd === 'pokemon') {
    const { routes } = JSON.parse(args[1]);
    search.__setDeps({
      safeFetchJson: async (url) => {
        calls += 1;
        const hit = routes[url.replace(search.POKEAPI_BASE, '')];
        if (hit === undefined) throw new SafeFetchError('HTTP_ERROR', 404);
        return structuredClone(hit);
      },
    });
    answer(await outcome(search.getPokemon(search.pokemonSlug(args[0]))));
  } else if (cmd === 'index') {
    search.__setDeps({ safeFetchJson: async () => { calls += 1; throw new SafeFetchError('NETWORK', 'offline'); } });
    answer({ entries: await search.getPokemonIndex(), calls });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
await redis.ping().catch(() => {}); // a pause write is fire-and-forget: let it land first
search.__resetState();
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
