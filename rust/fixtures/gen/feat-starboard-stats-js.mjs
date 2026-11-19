// The JS side of feat-starboard-stats' tests: the JS bot's own starboardService and statsService, against the test
// servers (TEST_DATABASE_URL, TEST_REDIS_URL) for the database commands. Prints one `RESULT <json>` line.
//
// Offline (no servers):
//   names <json [string]>                         normalizeBoardName of each
//   emoji <json { inputs, emojis }>               parseEmojiList of each input, with a guild holding `emojis` [{ id, name }]
//   unicode <json [string]>                       isUnicodeEmoji of each
//   payload <json [{ guildId, message, count, emojiText }]>
//                                                 buildRepostPayload of a raw Discord message, as API JSON
//   summary <json [board]>                        settingsSummary of each (no client)
//   templates <json [{ input, kind, roleName }]>  validateStatTemplate, and renderStatName at 1234 / null
// Database:
//   sb-create <guildId> <json data>               ensureGuildSettings + createBoard, then getBoards (fills the cache)
//   sb-boards <guildId>                           getBoards (cache, then Postgres)
//   sb-board <guildId> <name>                     getBoardByName
//   sb-update <guildId> <boardId> <json patch>    updateBoard
//   sb-sync <guildId> <json world>                syncMessage against fake channels; returns the Discord calls
//   sb-entries <boardId>                          the board's StarboardEntry rows
//   sb-search <guildId> <json [string]>           searchBoards of each query (the board names found)
//   st-create <guildId> <json opts>               createStatChannel against a fake guild
//   st-rows <guildId>                             loadRows
//   st-job <guildId>                              the stats.refresh job row
//   st-ensure <guildId> <runAtMs>                 ensureRefreshJob
//   st-refresh <guildId> <json guild>             refreshGuild of loadRows against a fake guild
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
// The logger prints through console; keep stdout clean for the RESULT line.
console.log = console.info = console.warn = console.debug = console.error = () => {};

const { Attachment, Client, Collection, Embed, Sticker, User } = await import('discord.js');
const sb = await imp('src', 'services', 'starboardService.js');
const stats = await imp('src', 'services', 'statsService.js');
const { ensureGuildSettings } = await imp('src', 'services', 'settingsService.js');
const { getJob } = await imp('src', 'services', 'scheduler.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis } = await imp('src', 'services', 'redis.js');

const client = new Client({ intents: [] });

/** A raw Discord message as the parts buildRepostPayload reads, built from discord.js' own structures. */
function messageOf(guildId, raw) {
  const author = new User(client, raw.author);
  const member = raw.member
    ? {
      displayName: raw.member.nick ?? author.displayName,
      displayAvatarURL: (opts) => (raw.member.avatar
        ? client.rest.cdn.guildMemberAvatar(guildId, author.id, raw.member.avatar, opts)
        : author.displayAvatarURL(opts)),
    }
    : null;
  return {
    id: raw.id,
    channelId: raw.channel_id,
    url: `https://discord.com/channels/${guildId}/${raw.channel_id}/${raw.id}`,
    content: raw.content,
    author,
    member,
    attachments: new Collection((raw.attachments ?? []).map((a) => [a.id, new Attachment(a)])),
    embeds: (raw.embeds ?? []).map((e) => new Embed(e)),
    stickers: new Collection((raw.sticker_items ?? []).map((s) => [s.id, new Sticker(client, s)])),
    createdTimestamp: Date.parse(raw.timestamp),
  };
}

function apiPayload(p) {
  return {
    content: p.content,
    embeds: p.embeds.map((e) => e.toJSON()),
    components: p.components.map((r) => r.toJSON()),
    allowed_mentions: p.allowedMentions,
  };
}

// ── Fakes for the sync engine (as in test/starboard.test.js) ─────────────────

const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });
let seq = 0;

function syncWorld(guildId, w) {
  const log = { sent: [], edits: [], deletes: [] };
  const guild = { id: guildId, available: true, emojis: { cache: new Collection() }, channels: { cache: new Map() }, members: { me: { id: w.botId } } };
  const fakeClient = {
    user: { id: w.botId },
    emojis: { cache: new Map() },
    channels: { fetch: async (id) => { const c = guild.channels.cache.get(id); if (!c) throw discordErr(10003); return c; } },
  };
  guild.client = fakeClient;
  const allPerms = () => ({ has: () => true });
  const base = (id) => ({
    id, guildId, guild, nsfw: false, parentId: null, name: `chan-${id.slice(-3)}`,
    isTextBased: () => true, isThread: () => false, permissionsFor: allPerms, toString() { return `<#${id}>`; },
  });
  const boardChannel = {
    ...base(w.boardChannelId),
    send: async (payload) => {
      const m = { id: String(BigInt(w.firstSentId ?? '600000000000000000') + BigInt(++seq)), payload, delete: async () => { log.deletes.push(m.id); }, react: async () => {} };
      log.sent.push({ id: m.id, payload: apiPayload(payload) });
      return m;
    },
    messages: {
      cache: new Map(),
      edit: async (id, payload) => { log.edits.push({ id, content: payload.content }); },
      delete: async (id) => { log.deletes.push(id); },
    },
  };
  const userList = (ids) => ids.map((id) => ({ id, bot: false }));
  const reactions = new Collection();
  const users = userList(w.userIds);
  reactions.set('⭐', {
    emoji: { id: null, name: '⭐', toString() { return '⭐'; } },
    count: users.length,
    me: false,
    countDetails: { burst: 0, normal: users.length },
    users: { fetch: async ({ limit = 100, after }) => new Collection(users.filter((u) => after === undefined || BigInt(u.id) > BigInt(after)).slice(0, limit).map((u) => [u.id, u])) },
  });
  const message = {
    ...messageOf(guildId, w.message),
    reactions: { cache: reactions },
    system: false,
    partial: false,
  };
  const source = { ...base(w.sourceChannelId), messages: { cache: new Map([[message.id, message]]), fetch: async () => message } };
  guild.channels.cache.set(boardChannel.id, boardChannel);
  guild.channels.cache.set(source.id, source);
  return { log, guild, client: fakeClient, source, message };
}

// ── A fake guild for the counters (as in test/stat-channels.test.js) ─────────

function statsGuild(id, spec = {}) {
  const members = new Map();
  for (let i = 0; i < (spec.humans ?? 10); i++) members.set(`h${i}`, { user: { bot: false }, roles: { cache: new Set(spec.humanRoles ?? []) } });
  for (let i = 0; i < (spec.bots ?? 0); i++) members.set(`b${i}`, { user: { bot: true }, roles: { cache: new Set() } });
  const all = { has: () => true };
  const guild = {
    id,
    name: 'Test Guild',
    available: true,
    memberCount: spec.memberCount ?? members.size,
    premiumSubscriptionCount: 0,
    members: { cache: members, me: { id: spec.botId ?? '900000000000000001', permissions: all } },
    presences: { cache: new Map() },
    roles: { cache: new Map([[id, { id, name: '@everyone' }], ...(spec.roles ?? []).map((r) => [r.id, r])]) },
    channels: { cache: new Map() },
    fetch: async () => ({ approximatePresenceCount: null }),
    calls: { created: [], renamed: [] },
  };
  const addChannel = (cid, name) => {
    const ch = {
      id: cid, name, type: 2, guild, isThread: () => false, permissionsFor: () => all,
      setName: async (n) => { guild.calls.renamed.push([cid, n]); ch.name = n; return ch; },
      delete: async () => { guild.channels.cache.delete(cid); },
    };
    guild.channels.cache.set(cid, ch);
    return ch;
  };
  for (const c of spec.channels ?? []) addChannel(c.id, c.name);
  guild.channels.create = async (opts) => { guild.calls.created.push({ ...opts, permissionOverwrites: opts.permissionOverwrites.map((o) => ({ ...o, allow: (o.allow ?? []).map(String), deny: (o.deny ?? []).map(String) })) }); return addChannel(spec.newChannelId ?? '955022019999999999', opts.name); };
  guild.channels.fetch = async (cid) => { const c = guild.channels.cache.get(cid); if (c) return c; throw discordErr(10003); };
  guild.channels.delete = async (cid) => { guild.channels.cache.delete(cid); };
  return guild;
}

const [cmd, ...args] = process.argv.slice(2);
// The database commands read and write the cache: wait for Redis (at most 3 s) so a first read is not a miss.
if (cmd.startsWith('sb-') || cmd.startsWith('st-')) {
  await new Promise((r) => { if (redis.status === 'ready') r(); else { redis.once('ready', r); setTimeout(r, 3000); } });
}
try {
  if (cmd === 'names') {
    answer(JSON.parse(args[0]).map((s) => sb.normalizeBoardName(s)));
  } else if (cmd === 'emoji') {
    const { inputs, emojis } = JSON.parse(args[0]);
    const guild = { emojis: { cache: new Collection((emojis ?? []).map((e) => [e.id, e])) } };
    answer(inputs.map((s) => sb.parseEmojiList(s, guild)));
  } else if (cmd === 'unicode') {
    answer(JSON.parse(args[0]).map((s) => sb.isUnicodeEmoji(s)));
  } else if (cmd === 'payload') {
    answer(JSON.parse(args[0]).map((c) => apiPayload(sb.buildRepostPayload({ message: messageOf(c.guildId, c.message), count: c.count, emojiText: c.emojiText }))));
  } else if (cmd === 'summary') {
    answer(JSON.parse(args[0]).map((b) => sb.settingsSummary(b, b.reposts === undefined ? {} : { reposts: b.reposts })));
  } else if (cmd === 'templates') {
    answer(JSON.parse(args[0]).map((c) => ({
      check: stats.validateStatTemplate(c.input, { kind: c.kind, roleName: c.roleName ?? null }),
      name: stats.renderStatName(c.input, { count: 1234, roleName: c.roleName ?? null }),
      loading: stats.renderStatName(c.input, { count: null, roleName: c.roleName ?? null }),
    })));
  } else if (cmd === 'sb-create') {
    await ensureGuildSettings(args[0]);
    const board = await sb.createBoard(args[0], JSON.parse(args[1]));
    answer({ board, boards: await sb.getBoards(args[0]) });
  } else if (cmd === 'sb-boards') {
    answer(await sb.getBoards(args[0]));
  } else if (cmd === 'sb-board') {
    answer(await sb.getBoardByName(args[0], args[1]));
  } else if (cmd === 'sb-update') {
    answer(await sb.updateBoard(args[0], args[1], JSON.parse(args[2])));
  } else if (cmd === 'sb-sync') {
    const w = syncWorld(args[0], JSON.parse(args[1]));
    await sb.syncMessage({ client: w.client, guild: w.guild, channel: w.source, messageId: w.message.id, ensureMessage: async () => w.message });
    answer(w.log);
  } else if (cmd === 'sb-search') {
    const out = [];
    for (const q of JSON.parse(args[1])) out.push((await sb.searchBoards(args[0], q)).map((b) => b.name));
    answer(out);
  } else if (cmd === 'sb-entries') {
    answer(await prisma.starboardEntry.findMany({ where: { starboardId: args[0] }, orderBy: { createdAt: 'asc' } }));
  } else if (cmd === 'st-create') {
    const o = JSON.parse(args[1]);
    const guild = statsGuild(args[0], o.guild ?? {});
    const role = o.roleId ? { id: o.roleId, name: o.roleName } : null;
    const made = await stats.createStatChannel({
      guild, client: { user: { id: o.guild?.botId ?? '900000000000000001' } }, kind: o.kind, template: o.template, role, category: null, actorTag: 'admin',
    });
    answer({ row: made.row, value: made.value, created: guild.calls.created, job: await getJob(stats.refreshJobKey(args[0])) });
  } else if (cmd === 'st-rows') {
    answer(await stats.loadRows(args[0]));
  } else if (cmd === 'st-job') {
    answer(await getJob(stats.refreshJobKey(args[0])));
  } else if (cmd === 'st-ensure') {
    answer({ ok: await stats.ensureRefreshJob(args[0], new Date(Number(args[1]))), job: await getJob(stats.refreshJobKey(args[0])) });
  } else if (cmd === 'st-refresh') {
    const guild = statsGuild(args[0], JSON.parse(args[1]));
    const rows = await stats.loadRows(args[0]);
    const result = await stats.refreshGuild(guild, rows, { allowFetch: false });
    answer({ result: { ...result, nextRunAt: result.nextRunAt.getTime() }, renamed: guild.calls.renamed, rows: await stats.loadRows(args[0]) });
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
