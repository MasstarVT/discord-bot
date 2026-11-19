// The JS side of feat-logging's cross-language tests: the JS bot's own services against the test servers
// (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-logging-js.mjs setnx <key>
//        cache.setNX(key, 1, 60) as the logging hook does it: { fresh, raw }
//   node rust/fixtures/gen/feat-logging-js.mjs track <guildId>
//        inviteTracker: refreshSnapshot, a matched join (code A by U1), an unknown join, a leave, a personal link
//   node rust/fixtures/gen/feat-logging-js.mjs read <guildId> <inviterId> <userId>
//        what the JS reads back: the stored snapshot (isSnapshot), getInviteStats, getJoinRecord, getPersonalInvite,
//        getInviteLeaderboard
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const inv = await imp('src', 'services', 'inviteTracker.js');
await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

function fakeGuild(id, invites) {
  const g = {
    id, available: true, vanityURLCode: null,
    live: new Map(invites.map((i) => [i.code, { maxUses: 0, uses: 0, inviterId: null, expiresTimestamp: null, ...i }])),
    client: { readyTimestamp: 0, user: { id: 'BOT' }, fetchInvite: async () => { throw Object.assign(new Error('Unknown Invite'), { code: 10006 }); } },
    members: { me: { permissions: { has: () => true } } },
    invites: { fetch: async () => new Map(g.live), delete: async () => {} },
    fetchVanityData: async () => null,
  };
  return g;
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'setnx') {
    const fresh = await cache.setNX(args[0], 1, 60);
    answer({ fresh, raw: await redis.get(args[0]) });
  } else if (cmd === 'track') {
    const gid = args[0];
    const g = fakeGuild(gid, [{ code: 'A', uses: 0, inviterId: 'U1' }, { code: 'ONCE', uses: 0, maxUses: 1, inviterId: 'U2' }]);
    const refreshed = await inv.refreshSnapshot(g);
    g.live.get('A').uses = 1;
    const joined = await inv.trackJoin({ id: 'm1', guild: g, joinedAt: new Date(Date.now() - 3 * 86_400_000), user: { id: 'm1', bot: false } });
    const unknown = await inv.trackJoin({ id: 'm2', guild: g, joinedAt: new Date(), user: { id: 'm2', bot: false } });
    await inv.trackLeave(g, 'm2');
    await inv.savePersonalInvite({ guildId: gid, userId: 'U1', code: 'JSLINK', channelId: '700000000000000001' });
    answer({ refreshed, joined, unknown, raw: await cache.get(inv.snapshotKey(gid)) });
  } else if (cmd === 'read') {
    const [gid, inviter, user] = args;
    const raw = await cache.get(inv.snapshotKey(gid));
    answer({
      raw,
      isSnapshot: inv.isSnapshot(raw),
      stats: await inv.getInviteStats(gid, inviter),
      record: await inv.getJoinRecord(gid, user),
      personal: await inv.getPersonalInvite(gid, inviter),
      leaderboard: await inv.getInviteLeaderboard(gid, { limit: 10, excludeIds: [] }),
    });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
