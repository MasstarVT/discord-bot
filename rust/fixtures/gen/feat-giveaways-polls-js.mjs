// The JS side of feat-giveaways-polls' cross-language tests: the JS bot's own giveawayService and pollService against
// the test database (TEST_DATABASE_URL / TEST_REDIS_URL), with just enough fake discord.js objects for the flows.
// Prints one `RESULT <json>` line. Message bodies are what discord.js would send (MessagePayload.resolveBody).
//
//   gstart <guildId> <channelId> <json opts>        startGiveaway (opts: prize, winnerCount, endsAtMs, roleWeights,
//                                                   requiredRoleIds, deniedRoleIds, xpPrize, coinPrize, description)
//   gjoin <guildId> <giveawayId> <userId> <json roles>  handleJoinButton (a Participate click)
//   gread <giveawayId>                              the row, its entries and its end job, as Prisma reads them
//   greroll <guildId> <giveawayId> <json members>   rerollGiveaway (members: [[id, [roles]]] in the member cache)
//   gend <guildId> <giveawayId> <json members>      endGiveaway (source: command) with a message that exists
//   pstart <guildId> <channelId> <json pending> <json input>  postPoll
//   pvote <pollId> <userId> <maxChoices> <json {idx}|{set}>   applyVote
//   pread <pollId>                                  the row, its tally and its end job
//   pend <guildId> <pollId>                         endPoll (reason manual) with an unavailable guild (no Discord calls)
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
console.error = console.warn = console.log = console.debug = console.info = () => {};

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
const { MessagePayload, ChannelType } = await imp('node_modules', 'discord.js', 'src', 'index.js');
const gs = await imp('src', 'services', 'giveawayService.js');
const ps = await imp('src', 'services', 'pollService.js');
const { getJob } = await imp('src', 'services', 'scheduler.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const target = { client: { options: { allowedMentions: { parse: ['users'], repliedUser: false } } }, messages: { resolveId: (x) => x } };
const body = (opts) => MessagePayload.create(target, opts).resolveBody().body;
const BOT = '600000000000000999';
let seq = 0;
const nextId = () => `7${String(Date.now()).slice(-9)}${String(++seq).padStart(8, '0')}`;

function makeGuild(id, { roles = [], members = [] } = {}) {
  const evaluate = (mid, roleIds) => ({ id: mid, user: { id: mid, bot: false }, partial: false, roles: { cache: new Set(roleIds) } });
  return {
    id,
    available: true,
    roles: { cache: new Map(roles.map((r) => [r, { id: r }])) },
    channels: { cache: new Map(), async fetch(cid) { throw Object.assign(new Error(`Unknown Channel ${cid}`), { code: 10003 }); } },
    members: {
      cache: new Map(members.map(([mid, r]) => [mid, evaluate(mid, r)])),
      me: { id: BOT },
      async fetch() { return new Map(); },
    },
  };
}

function makeChannel(guild, id) {
  const store = new Set();
  const ch = {
    id, name: `chan-${id}`, type: ChannelType.GuildText, guild, sent: [], edits: [],
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: () => true }),
    toString: () => `<#${id}>`,
    async send(payload) {
      const mid = nextId();
      store.add(mid);
      ch.sent.push(body(payload));
      return { id: mid, url: `https://discord.com/channels/${guild.id}/${id}/${mid}`, async delete() { store.delete(mid); } };
    },
    messages: {
      async fetch(mid) { return { id: mid }; },
      async edit(mid, payload) { ch.edits.push({ id: mid, body: body(payload) }); return { id: mid }; },
    },
  };
  guild.channels.cache.set(id, ch);
  return ch;
}

const jobOf = async (key) => {
  const j = await getJob(key);
  return j ? { type: j.type, dedupeKey: j.dedupeKey, payload: j.payload, runAt: j.runAt, maxAttempts: j.maxAttempts, status: j.status, guildId: j.guildId } : null;
};

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'gstart') {
    const [guildId, channelId, raw] = args;
    const o = JSON.parse(raw);
    const guild = makeGuild(guildId, { roles: o.roles ?? [] });
    const channel = makeChannel(guild, channelId);
    const res = await gs.startGiveaway({
      guild, channel, hostId: '600000000000000001', prize: o.prize, description: o.description ?? null, winnerCount: o.winnerCount ?? 1,
      endsAt: new Date(o.endsAtMs), requiredRoleIds: o.requiredRoleIds ?? [], deniedRoleIds: o.deniedRoleIds ?? [], roleWeights: o.roleWeights ?? [],
      xpPrize: o.xpPrize ?? null, coinPrize: o.coinPrize ?? null, settings: null,
    });
    if (!res.ok) throw new Error(`startGiveaway: ${res.code}`);
    answer({ giveaway: res.giveaway, sent: channel.sent, job: await jobOf(gs.endJobKey(res.giveaway.id)) });
  } else if (cmd === 'gjoin') {
    const [guildId, giveawayId, userId, rolesRaw] = args;
    const roles = JSON.parse(rolesRaw);
    const guild = makeGuild(guildId, { roles });
    const replies = [];
    const i = {
      customId: `giveaway:join:${giveawayId}`, user: { id: userId }, guildId, guild,
      member: { id: userId, roles: { cache: new Set(roles) } },
      deferred: false, replied: false, ephemeral: null, createdTimestamp: Date.now(), inCachedGuild: () => true,
      async deferReply({ ephemeral } = {}) { i.deferred = true; i.ephemeral = Boolean(ephemeral); },
      async editReply(p) { replies.push(p); return { id: 'r' }; },
      async followUp(p) { replies.push(p); return { id: 'f' }; },
      async reply(p) { i.replied = true; replies.push(p); return {}; },
    };
    await gs.handleJoinButton(i, { guilds: { cache: new Map() }, user: { id: BOT } });
    gs.resetCountRefresh();
    const embed = replies.at(-1)?.embeds?.[0];
    answer({ title: embed?.data?.title ?? embed?.title ?? replies.at(-1)?.content ?? null });
  } else if (cmd === 'gread') {
    const g = await prisma.giveaway.findUnique({ where: { id: args[0] } });
    const entries = await prisma.giveawayEntry.findMany({ where: { giveawayId: args[0] }, orderBy: { createdAt: 'asc' } });
    answer({ giveaway: g, entries, job: await jobOf(gs.endJobKey(args[0])) });
  } else if (cmd === 'greroll' || cmd === 'gend') {
    const [guildId, giveawayId, membersRaw] = args;
    const members = JSON.parse(membersRaw);
    const row = await prisma.giveaway.findUnique({ where: { id: giveawayId } });
    const guild = makeGuild(guildId, { members });
    const channel = makeChannel(guild, row.channelId);
    const res = cmd === 'greroll'
      ? await gs.rerollGiveaway({ guild, giveawayId, count: 1 })
      : await gs.endGiveaway({ client: { guilds: { cache: new Map([[guildId, guild]]) } }, guild, giveawayId, source: 'command' });
    answer({ res, sent: channel.sent, edits: channel.edits });
  } else if (cmd === 'pstart') {
    const [guildId, channelId, pendingRaw, inputRaw] = args;
    const guild = makeGuild(guildId);
    const channel = makeChannel(guild, channelId);
    const input = JSON.parse(inputRaw);
    const out = await ps.postPoll({ client: { guilds: { cache: new Map() } }, guild, channel, pending: JSON.parse(pendingRaw), ...input });
    answer({ out, sent: channel.sent, job: out.ok ? await jobOf(ps.endJobKey(out.poll.id)) : null });
  } else if (cmd === 'pvote') {
    const [pollId, userId, max, raw] = args;
    const v = JSON.parse(raw);
    answer(await ps.applyVote({ pollId, userId, maxChoices: Number(max), idx: v.idx ?? null, set: v.set ?? null }));
  } else if (cmd === 'pread') {
    const p = await prisma.poll.findUnique({ where: { id: args[0] } });
    const votes = await prisma.pollVote.findMany({ where: { pollId: args[0] }, orderBy: [{ userId: 'asc' }, { optionIndex: 'asc' }] });
    answer({ poll: p, tally: p ? await ps.getTally(p) : null, votes: votes.map((v) => [v.userId, v.optionIndex]), job: await jobOf(ps.endJobKey(args[0])) });
  } else if (cmd === 'pend') {
    const [guildId, pollId] = args;
    const r = await ps.endPoll({ guilds: { cache: new Map() } }, pollId, { reason: 'manual', guildId });
    answer(r);
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
