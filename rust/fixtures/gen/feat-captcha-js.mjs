// The JS side of feat-captcha's cross-language tests: the JS bot's own captchaService / stickyService against the test
// servers (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   join <guildId> <roleId> <userId> <minutes>   settings, then captchaService.onMemberJoin: { state, row, job }
//   challenge <guildId> <userId>                 captchaService.issueChallenge (a real image and code): the row
//   check <guildId> <userId> <input>             the stored row and captchaService.evaluateAnswer of input
//   leave <guildId> <userId>                     captchaService.onMemberLeave, then { pending, job }
//   job <dedupeKey>                              scheduler.getJob
//   count <guildId>                              captchaService.countPending
//   settings <guildId>                           the GuildSettings captcha fields through settingsService.getSettings
//   sticky-set <guildId> <channelId> <json args> stickyService.setSticky with fake Discord objects: { result, row }
//   sticky-get <guildId> <channelId>             stickyService.getSticky, listStickies and getChannels
//   window <key> <n>                             cache.slidingWindow(key, 60000) n times: the last count
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);
console.error = console.warn = console.log = console.debug = console.info = () => {};

const svc = await imp('src', 'services', 'captchaService.js');
const sticky = await imp('src', 'services', 'stickyService.js');
const settingsService = await imp('src', 'services', 'settingsService.js');
const scheduler = await imp('src', 'services', 'scheduler.js');
const { cache, default: redis } = await imp('src', 'services', 'redis.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { Collection } = await import('discord.js');

// The cache fails open while it connects: wait until Redis is ready (when a Redis URL is given).
if (process.env.TEST_REDIS_URL) await new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r)));

const rowOf = (guildId, userId) => prisma.captchaVerification.findUnique({ where: { guildId_userId: { guildId, userId } } });

function fakeMember(guild, id, joined = Date.now()) {
  return {
    id, guild, pending: false, joinedTimestamp: joined, user: { id, bot: false, tag: `u${id}#0` },
    roles: { cache: new Collection(), add: async () => { throw Object.assign(new Error('offline'), { code: 50013 }); } },
  };
}

function stickyWorld(guildId, channelId) {
  const channel = {
    id: channelId, guildId, sent: [], lastMessageId: undefined,
    permissionsFor: () => ({ has: () => true }), isTextBased: () => true, isThread: () => false,
    messages: { delete: async () => {} },
    async send(payload) { const m = { id: String(BigInt(channelId) + 1000n + BigInt(channel.sent.length)) }; channel.sent.push(payload); channel.lastMessageId = m.id; return m; },
  };
  const guild = { id: guildId, name: 'Sticky Guild', memberCount: 5, channels: { cache: new Map([[channelId, channel]]) }, members: { me: {} } };
  channel.guild = guild;
  const client = { user: { id: 'BOT' }, guilds: { cache: new Map([[guildId, guild]]) } };
  return { client, guild, channel };
}

const [cmd, ...args] = process.argv.slice(2);
try {
  if (cmd === 'join') {
    const [g, role, u, minutes] = args;
    await settingsService.updateSettings(g, { captchaEnabled: true, captchaRoleId: role, captchaTimeoutMinutes: Number(minutes) });
    const guild = { id: g, roles: { cache: new Collection([[role, { id: role }]]) } };
    const ctx = { guild, guildId: g, member: fakeMember(guild, u), getSettings: () => settingsService.getSettings(g), state: {} };
    await svc.onMemberJoin(ctx);
    answer({ state: ctx.state, row: await rowOf(g, u), job: await scheduler.getJob(svc.timeoutJobKey(g, u)) });
  } else if (cmd === 'challenge') {
    const [g, u] = args;
    const settings = await settingsService.getSettings(g);
    const payload = await svc.issueChallenge(fakeMember({ id: g }, u), settings);
    answer({ row: await rowOf(g, u), files: payload.files.length });
  } else if (cmd === 'check') {
    const [g, u, input] = args;
    const row = await rowOf(g, u);
    answer({ row, verdict: svc.evaluateAnswer({ row, guildId: g, userId: u, input }) });
  } else if (cmd === 'leave') {
    const [g, u] = args;
    await svc.onMemberLeave({ guildId: g, user: { id: u } });
    answer({ pending: await svc.countPending(g), job: await scheduler.getJob(svc.timeoutJobKey(g, u)) });
  } else if (cmd === 'job') {
    answer(await scheduler.getJob(args[0]));
  } else if (cmd === 'count') {
    answer(await svc.countPending(args[0]));
  } else if (cmd === 'settings') {
    const s = await settingsService.getSettings(args[0], { fresh: true });
    const pick = Object.fromEntries(Object.entries(s).filter(([k]) => k.startsWith('captcha')));
    answer({ ...pick, panel: svc.panelTemplate(s), button: svc.parseVerifyButton(s.captchaTemplate?.verifyButton) });
  } else if (cmd === 'sticky-set') {
    const [g, c, raw] = args;
    const { client, guild, channel } = stickyWorld(g, c);
    await settingsService.updateSettings(g, { stickyEnabled: true });
    const result = await sticky.setSticky({ client, guild, channel, userId: '600000000000000001', ...JSON.parse(raw) });
    answer({ result, row: await sticky.getSticky(g, c), sent: channel.sent.length });
  } else if (cmd === 'sticky-get') {
    const [g, c] = args;
    sticky.invalidate(g);
    const channels = await sticky.getChannels(g);
    answer({ row: await sticky.getSticky(g, c), list: await sticky.listStickies(g), channels: channels && Object.fromEntries(channels), count: await sticky.countStickies(g) });
  } else if (cmd === 'window') {
    const [key, n] = args;
    let last = null;
    for (let i = 0; i < Number(n); i++) last = await cache.slidingWindow(key, 60_000);
    answer(last);
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
sticky._scheduler.clear();
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
