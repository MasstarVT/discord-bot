// The JS side of feat-community-a's cross-language tests: the JS bot's own suggestion, confession and bump services against
// the test database (TEST_DATABASE_URL) and Redis (TEST_REDIS_URL). Discord is replaced by the same kind of stubs the JS tests
// use. Prints one `RESULT <json>` line.
//
//   bump-record <guildId> <channelId> <bumperId|-> <atIso>   bumpService.recordBump (real scheduleJob)
//   bump-read <guildId> <userId>                           getBumpStates, topBumpers, bumpCountOf and the reminder job row
//   bump-reset <guildId> [userId]                          bumpService.resetCounts
//   sugg-create <guildId> <channelId> <authorId> <content> <anonymous 0|1> [cooldownSeconds]
//                                                          suggestionService.createSuggestion (fake channel)
//   sugg-vote <suggestionId> <userId> <1|-1>               suggestionService.applyVote
//   sugg-read <guildId> <number>                           the Suggestion row (Prisma) and its votes
//   sugg-review <guildId> <number> <status> <note|-> <reviewerId>
//                                                          suggestionService.reviewSuggestion (no Discord)
//   conf-create <guildId> <channelId> <reviewChannelId|-> <authorId> <content> [cooldownSeconds]
//                                                          confessionService.createConfession (fake channels)
//   conf-read <guildId> <number>                           the Confession row (Prisma) and the blocklist
//   conf-block <guildId> <number> <0|1>                    confessionService.setBlocked
//   conf-delete <guildId> <number>                         confessionService.deleteConfession (no posts to delete)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
for (const k of ['log', 'info', 'warn', 'debug']) console[k] = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const bump = await imp('src', 'services', 'bumpService.js');
const sug = await imp('src', 'services', 'suggestionService.js');
const conf = await imp('src', 'services', 'confessionService.js');
const settingsSvc = await imp('src', 'services', 'settingsService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

let nextId = 700000000000000100n;
const snow = () => String(nextId++);

/** A text channel that lets the bot do everything; `send` returns a message with an id. */
function fakeChannel(id, guildId) {
  const ch = {
    id, guildId, sent: [], type: 0,
    guild: { members: { me: {} } },
    permissionsFor: () => ({ has: () => true }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) {
      const message = { id: snow(), async delete() {}, async startThread() { return { id: snow() }; } };
      ch.sent.push(payload);
      return message;
    },
    messages: { async edit() {}, async delete() {}, async fetch() { return null; } },
  };
  return ch;
}

function fakeWorld(guildId, channels) {
  const cache = new Map(channels.map((c) => [c.id, c]));
  const guild = {
    id: guildId, name: 'Test Guild', available: true,
    channels: { cache, fetch: async (id) => cache.get(id) ?? null },
    members: { cache: new Map(), me: {} },
  };
  const client = {
    user: { id: 'BOT' },
    guilds: { cache: new Map([[guildId, guild]]) },
    channels: { cache, fetch: async (id) => { const c = cache.get(id); if (!c) throw Object.assign(new Error('Unknown Channel'), { code: 10003 }); return c; } },
    users: { cache: new Map(), fetch: async () => { throw new Error('no user'); } },
  };
  return { guild, client };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'bump-record') {
    const [guildId, channelId, bumperId, atIso] = args;
    const out = await bump.recordBump({ guildId, site: 'disboard', channelId, bumperId: bumperId === '-' ? null : bumperId, at: new Date(atIso) });
    answer({ out });
  } else if (cmd === 'bump-read') {
    const [guildId, userId] = args;
    answer({
      states: await bump.getBumpStates(guildId),
      top: await bump.topBumpers(guildId),
      own: await bump.bumpCountOf(guildId, userId),
      job: await prisma.scheduledJob.findUnique({ where: { dedupeKey: bump.reminderDedupeKey(guildId, 'disboard') } }),
    });
  } else if (cmd === 'bump-reset') {
    answer({ removed: await bump.resetCounts(args[0], args[1] ?? null) });
  } else if (cmd === 'sugg-create') {
    const [guildId, channelId, authorId, content, anon, cooldown] = args;
    const channel = fakeChannel(channelId, guildId);
    const { guild, client } = fakeWorld(guildId, [channel]);
    const settings = {
      suggestionsEnabled: true, suggestionChannelId: channelId, suggestionThreads: false, suggestionAllowAnonymous: true,
      suggestionDmOnUpdate: false, suggestionCooldownSeconds: Number(cooldown ?? 0),
    };
    const out = await sug.createSuggestion({ client, guild, user: { id: authorId }, content, anonymous: anon === '1', settings });
    answer({ out, sent: channel.sent.map((p) => ({ embeds: p.embeds.map((e) => { const j = e.toJSON(); delete j.timestamp; return j; }), components: p.components.map((r) => r.toJSON()), allowedMentions: p.allowedMentions })) });
  } else if (cmd === 'sugg-vote') {
    const [suggestionId, userId, value] = args;
    answer(await sug.applyVote({ suggestionId, userId, value: Number(value) }));
  } else if (cmd === 'sugg-read') {
    const [guildId, number] = args;
    const row = await prisma.suggestion.findUnique({ where: { guildId_number: { guildId, number: Number(number) } } });
    const votes = row ? await prisma.suggestionVote.findMany({ where: { suggestionId: row.id }, orderBy: { userId: 'asc' } }) : [];
    answer({ row, votes });
  } else if (cmd === 'sugg-review') {
    const [guildId, number, status, note, reviewerId] = args;
    const { client, guild } = fakeWorld(guildId, []);
    const out = await sug.reviewSuggestion({ client, guild, number: Number(number), status, note: note === '-' ? null : note, reviewer: { id: reviewerId }, settings: { suggestionDmOnUpdate: false } });
    answer(out);
  } else if (cmd === 'conf-create') {
    const [guildId, channelId, reviewId, authorId, content, cooldown] = args;
    const post = fakeChannel(channelId, guildId);
    const review = reviewId === '-' ? null : fakeChannel(reviewId, guildId);
    const { guild, client } = fakeWorld(guildId, [post, ...(review ? [review] : [])]);
    const settings = {
      confessionsEnabled: true, confessionChannelId: channelId, confessionReviewChannelId: review ? reviewId : null,
      confessionCooldownSeconds: Number(cooldown ?? 0), confessionAllowImages: false, confessionBlockLinks: false, confessionBlockedUserIds: [],
    };
    const out = await conf.createConfession({ client, guild, user: { id: authorId }, content, settings });
    answer({ out, posted: post.sent.length, reviewed: review?.sent.length ?? 0 });
  } else if (cmd === 'conf-read') {
    const [guildId, number] = args;
    const row = await prisma.confession.findUnique({ where: { guildId_number: { guildId, number: Number(number) } } });
    const s = await prisma.guildSettings.findUnique({ where: { guildId }, select: { confessionBlockedUserIds: true } });
    answer({ row, blocked: s?.confessionBlockedUserIds ?? [] });
  } else if (cmd === 'conf-block') {
    const [guildId, number, blocked] = args;
    answer(await conf.setBlocked({ guildId, number: Number(number), blocked: blocked === '1' }));
  } else if (cmd === 'conf-delete') {
    const [guildId, number] = args;
    const { client, guild } = fakeWorld(guildId, []);
    answer(await conf.deleteConfession({ client, guild, number: Number(number), settings: null }));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.message ?? err) });
}
void settingsSvc;
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
