// pkg-starboard tests (pkg-starboard.md §9 plus the architect amendments). Offline by default: name/emoji parsing, filters,
// counting, the decision table (incl. blocked entries), the repost payload, the coalescer and semaphore, the plugin declaration,
// the command payload and the reaction hook's early exits. With TEST_DATABASE_URL (a migrated database) the DB cases also run:
// the sync engine end to end against fake Discord objects (create / update / remove / repost / blocked / 10008), mirrored edits,
// source/repost/bulk/channel deletes, the cap under the row lock, updateBoard, listBoardsWithCounts, every RAW_SQL statement
// and the /starboard subcommands against fake interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/starboard.test.js
// NOTE: the database tests run on one Postgres connection (connection_limit=1), so concurrent creates are serialised by the pool.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open (every call falls through to Postgres).
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { Collection, PermissionFlagsBits, ReactionType, StickerFormatType } = await import('discord.js');
const sb = await import('../src/services/starboardService.js');
const cmd = await import('../src/commands/admin/starboard.js');
const pluginMod = await import('../src/plugins/starboard.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { createKeyedMutex } = await import('../src/utils/async.js');
const { markBotDeletion } = await import('../src/utils/deletionTracker.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `95501800000000${String(n).padStart(4, '0')}`; // guild ids unique to this package (port 55018)
const ALL_GUILDS = Array.from({ length: 60 }, (_, i) => G(i + 1));

after(async () => {
  Object.assign(console, quiet);
  sb.__setSyncImpl();
  redis.disconnect();
  if (DB) {
    // The cascade removes Starboard and StarboardEntry rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const sid = (prefix, n) => `${prefix}${String(n).padStart(18 - prefix.length, '0')}`; // 18-digit snowflake-like id
const USER = (n) => sid('7', n);
const BOT = sid('6', 1);
let seq = 0;
const nextId = () => sid('5', ++seq);
/** A snowflake whose creation time is `ts` (ms). */
const snowflakeAt = (ts) => String((BigInt(ts) - 1420070400000n) << 22n);
/** A message id created "now" (the age checks read the id). */
const nextMessageId = () => String(BigInt(snowflakeAt(Date.now())) + BigInt(++seq));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const discordErr = (code) => Object.assign(new Error(`Discord ${code}`), { code });
async function until(fn, ms = 3000) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error('until: timed out');
    await sleep(15);
  }
}

const emoji = (id, name) => ({ id, name, toString() { return id ? `<:${name}:${id}>` : name; } });
const STAR = emoji(null, '⭐');
const GLOW = emoji(null, '🌟');
const POG = emoji('123456789012345678', 'pog');

const baseBoard = (over = {}) => ({
  id: 'b1', name: 'main', channelId: 'board-ch', emojis: ['⭐'], threshold: 3, selfStar: false, ignoreBots: true,
  removeBelowThreshold: true, autoReact: false, maxMessageAgeDays: 0, allowNsfw: false, ignoredChannelIds: [],
  removeSelfStars: false, enabled: true, ...over,
});

const fakeMsg = (over = {}) => ({
  id: '300000000000000001', channelId: 'c1', url: 'https://discord.com/channels/1/c1/300000000000000001', content: 'hello',
  author: { id: 'u1', bot: false, username: 'alice', displayName: 'Alice', displayAvatarURL: () => 'https://cdn.example/a.png' },
  member: null, attachments: new Map(), embeds: [], stickers: new Map(), reactions: { cache: new Collection() },
  createdTimestamp: 1_700_000_000_000, system: false, ...over,
});

// ── Fakes for the sync engine (just enough of discord.js) ────────────────────

const allPerms = () => ({ has: () => true });

function makeWorld(n, { boardNsfw = false } = {}) {
  const guildId = G(n);
  const log = { sent: [], edits: [], deletes: [], reacts: [], removed: [] };
  const world = { guildId, log, editError: null, deleteError: null, sendError: null };

  const client = {
    user: { id: BOT },
    emojis: { cache: new Map() },
    channels: {
      fetch: async (id) => {
        const c = world.guild.channels.cache.get(id);
        if (!c) throw discordErr(10003);
        return c;
      },
    },
  };
  const guild = {
    id: guildId, available: true, client, emojis: { cache: new Collection() },
    channels: { cache: new Map() }, members: { me: { id: BOT } },
  };
  const channelBase = (id, over = {}) => ({
    id, guildId, guild, nsfw: false, parentId: null, name: `chan-${id.slice(-3)}`,
    isTextBased: () => true, isThread: () => false, permissionsFor: allPerms,
    toString() { return `<#${id}>`; }, ...over,
  });
  const boardChannel = channelBase(sid('4', n * 10 + 1), {
    nsfw: boardNsfw,
    send: async (payload) => {
      if (world.sendError) throw world.sendError;
      const m = { id: nextId(), payload, deleted: false, delete: async () => { m.deleted = true; log.deletes.push(m.id); }, react: async (e) => { log.reacts.push(e); } };
      log.sent.push(m);
      return m;
    },
    messages: {
      cache: new Map(),
      edit: async (id, payload) => {
        if (world.editError) throw world.editError;
        log.edits.push({ id, payload });
      },
      delete: async (id) => {
        if (world.deleteError) throw world.deleteError;
        log.deletes.push(id);
      },
    },
  });
  const cache = new Map();
  const source = channelBase(sid('4', n * 10 + 2), {
    messages: {
      cache,
      fetch: async (id) => {
        if (!cache.has(id)) throw discordErr(10008);
        return cache.get(id);
      },
    },
  });
  guild.channels.cache.set(boardChannel.id, boardChannel);
  guild.channels.cache.set(source.id, source);
  Object.assign(world, { client, guild, boardChannel, source });

  /** A source message with reactions: [{ emoji, userIds, bots?, count?, me? }]. */
  world.message = ({ id = nextMessageId(), authorId = USER(1), bot = false, reactions = [], channel = source, ...rest } = {}) => {
    const rc = new Collection();
    for (const r of reactions) {
      const users = r.userIds.map((u) => ({ id: u, bot: (r.bots ?? []).includes(u) }));
      const reaction = {
        emoji: r.emoji, count: r.count ?? users.length, me: r.me ?? false, countDetails: { burst: 0, normal: users.length },
        users: {
          fetch: async ({ type, limit = 100, after }) => {
            reaction.fetches.push({ type, limit, after });
            const sorted = [...users].sort((a, b) => (BigInt(a.id) < BigInt(b.id) ? -1 : 1));
            const page = sorted.filter((u) => after === undefined || BigInt(u.id) > BigInt(after)).slice(0, limit);
            return new Collection(page.map((u) => [u.id, u]));
          },
          remove: async (uid) => { log.removed.push(uid); },
        },
        fetches: [],
      };
      rc.set(r.emoji.id ?? r.emoji.name, reaction);
    }
    const msg = fakeMsg({
      id, channelId: channel.id, url: `https://discord.com/channels/${guildId}/${channel.id}/${id}`,
      author: { id: authorId, bot, username: 'alice', displayName: 'Alice', displayAvatarURL: () => 'https://cdn.example/a.png' },
      reactions: { cache: rc }, createdTimestamp: Date.now(), partial: false, ...rest,
    });
    channel.messages?.cache.set(id, msg);
    return msg;
  };
  world.sync = (message, channel = source) => sb.syncMessage({
    client, guild, channel, messageId: message.id, ensureMessage: async () => message,
  });
  world.ctx = (extra = {}) => ({ client, guild, guildId, ...extra });
  return world;
}

async function setupGuild(n) {
  await ensureGuildSettings(G(n));
  await sb.invalidateBoards(G(n));
}
const mkBoard = (n, over = {}) => sb.createBoard(G(n), { name: 'main', channelId: sid('4', n * 10 + 1), ...over });
const entriesOf = (boardId) => prisma.starboardEntry.findMany({ where: { starboardId: boardId } });

// ═══════════════════════════ Offline cases ════════════════════════════════════

test('1. normalizeBoardName', () => {
  assert.equal(sb.normalizeBoardName('  Hall of Fame! '), 'hall-of-fame');
  assert.equal(sb.normalizeBoardName('***'), null);
  assert.equal(sb.normalizeBoardName(null), null);
  assert.equal(sb.normalizeBoardName('x'.repeat(40)).length, 32);
  assert.equal(sb.normalizeBoardName('a  b__c--d'), 'a-b__c-d');
});

test('2. parseEmojiList', () => {
  const ok = (s, guild) => { const r = sb.parseEmojiList(s, guild); assert.equal(r.ok, true, `${s}: ${r.error}`); return r.emojis; };
  assert.deepEqual(ok('⭐'), ['⭐']);
  assert.equal(ok('⭐ 🌟').length, 2);
  assert.equal(ok('⭐🌟').length, 2, 'no space needed');
  assert.deepEqual(ok('<:pog:123456789012345678>'), ['123456789012345678']);
  assert.deepEqual(ok('<a:x:123456789012345678>'), ['123456789012345678']);
  assert.deepEqual(ok('123456789012345678'), ['123456789012345678']);
  assert.deepEqual(ok('any'), []);
  assert.deepEqual(ok('ALL'), []);
  assert.deepEqual(ok('*'), []);
  assert.deepEqual(ok('⭐ ⭐'), ['⭐']);
  assert.deepEqual(ok('❤️ ❤'), ['❤️'], 'FE0F-insensitive, first spelling kept');
  assert.equal(ok('1️⃣ 🇬🇧 👨‍👩‍👧').length, 3);
  assert.equal(ok('👍🏽 🏳️‍🌈 ❤️‍🔥 🫠 🏴󠁧󠁢󠁥󠁮󠁧󠁿').length, 5);
  assert.deepEqual(ok('⭐, 🌟'), ['⭐', '🌟'], 'commas separate too');

  const guild = { emojis: { cache: new Collection([['100000000000000001', { id: '100000000000000001', name: 'pog' }]]) } };
  assert.deepEqual(ok(':pog:', guild), ['100000000000000001']);
  assert.deepEqual(ok(':POG:', guild), ['100000000000000001'], 'case-insensitive fallback');
  assert.equal(sb.parseEmojiList(':nope:', guild).ok, false);
  assert.match(sb.parseEmojiList(':nope:', guild).error, /:nope:/);
  const bad = sb.parseEmojiList('abc');
  assert.equal(bad.ok, false);
  assert.match(bad.error, /not an emoji/);
  assert.equal(sb.parseEmojiList('1').ok, false, 'a bare digit is not an emoji');
  assert.equal(sb.parseEmojiList('').ok, false);
  assert.equal(sb.parseEmojiList(' , ').ok, false);
  const many = ['⭐', '🌟', '💫', '✨', '🔥', '💎', '🎉', '👍', '👏', '🙌', '🏆'].join(' ');
  assert.equal(sb.parseEmojiList(many).ok, false);
  assert.match(sb.parseEmojiList(many).error, /at most 10/);
  assert.equal(sb.parseEmojiList(many.split(' ').slice(0, 10).join(' ')).ok, true);
});

test('2b. emoji keys, formatting and the unicode matcher', () => {
  assert.equal(sb.normalizeEmojiKey('❤️'), '❤');
  assert.equal(sb.emojiKeyOf({ id: '1', name: 'x' }), '1');
  assert.equal(sb.emojiKeyOf({ id: null, name: '⭐' }), '⭐');
  for (const e of ['⭐', '❤️', '❤', '👍🏽', '👨‍👩‍👧', '🇬🇧', '1️⃣', '🏴󠁧󠁢󠁥󠁮󠁧󠁿', '🏳️‍🌈', '❤️‍🔥', '🫠']) assert.equal(sb.isUnicodeEmoji(e), true, e);
  for (const e of ['a', '1', '#', '', 'ab']) assert.equal(sb.isUnicodeEmoji(e), false, JSON.stringify(e));
  assert.equal(sb.formatEmojiKey('⭐'), '⭐');
  assert.equal(sb.formatEmojiKey('123456789012345678'), '⭐', 'unresolvable id falls back');
  const message = fakeMsg({ reactions: { cache: new Collection([['123456789012345678', { emoji: POG }]]) } });
  assert.equal(sb.formatEmojiKey('123456789012345678', { message }), '<:pog:123456789012345678>');
  const client = { emojis: { cache: new Map([['123456789012345678', POG]]) } };
  assert.equal(sb.formatEmojiKey('123456789012345678', { client }), '<:pog:123456789012345678>');
  assert.equal(sb.formatEmojiList([], client), 'any emoji');
  assert.equal(sb.formatEmojiList(['⭐', '123456789012345678'], client), '⭐ <:pog:123456789012345678>');
});

test('3. boardMatchesEmoji', () => {
  assert.equal(sb.boardMatchesEmoji({ emojis: [] }, 'x'), true);
  assert.equal(sb.boardMatchesEmoji({ emojis: [] }, null), true);
  assert.equal(sb.boardMatchesEmoji({ emojis: ['❤️'] }, '❤'), true);
  assert.equal(sb.boardMatchesEmoji({ emojis: ['⭐'] }, '🌟'), false);
  assert.equal(sb.boardMatchesEmoji({ emojis: ['⭐'] }, null), false);
  assert.equal(sb.boardMatchesEmoji({ emojis: ['123456789012345678'] }, '123456789012345678'), true);
});

test('4. sourceSkipReason', () => {
  const ch = (over = {}) => ({ id: 'src', parentId: null, nsfw: false, isThread: () => false, ...over });
  const thread = (parent, over = {}) => ({ id: 'thr', parentId: parent.id, parent, nsfw: false, isThread: () => true, ...over });
  const board = baseBoard({ ignoredChannelIds: ['ign', 'cat-ignored'] });
  const ids = new Set(['board-ch', 'other-board']);
  const reason = (channel, b = board, boardChannel) => sb.sourceSkipReason({ board: b, channel, boardChannelIds: ids, boardChannel });

  assert.equal(reason(ch()), null);
  assert.equal(reason(ch({ id: 'board-ch' })), 'board-channel');
  assert.equal(reason(ch({ id: 'other-board' })), 'board-channel');
  assert.equal(reason(thread(ch({ id: 'board-ch' }))), 'board-channel');
  assert.equal(reason(ch({ id: 'ign' })), 'ignored');
  assert.equal(reason(ch({ parentId: 'cat-ignored' })), 'ignored');
  assert.equal(reason(thread(ch({ id: 'ign' }))), 'ignored');
  assert.equal(reason(thread(ch({ id: 'p', parentId: 'cat-ignored' }))), 'ignored', "a thread's category counts");
  assert.equal(reason(ch({ nsfw: true })), 'nsfw');
  assert.equal(reason(ch({ nsfw: true }), baseBoard({ allowNsfw: true }), { nsfw: false }), 'nsfw');
  assert.equal(reason(ch({ nsfw: true }), baseBoard({ allowNsfw: true }), undefined), 'nsfw');
  assert.equal(reason(ch({ nsfw: true }), baseBoard({ allowNsfw: true }), { nsfw: true }), null);
  assert.equal(reason(thread(ch({ nsfw: true }))), 'nsfw', 'an NSFW parent of a thread is respected');
  assert.equal(reason(thread(ch({ nsfw: false }), { nsfw: true })), null, "the thread's own flag is ignored, the parent decides");
});

test('5. messageSkipReason and snowflakeTime', () => {
  const now = 1_800_000_000_000;
  const board = baseBoard();
  const skip = (message, b = board, hasEntry = false) => sb.messageSkipReason({ board: b, message, hasEntry, now, botUserId: 'bot' });
  const msg = (over = {}) => ({ id: snowflakeAt(now - 1000), author: { id: 'u', bot: false }, system: false, webhookId: null, ...over });

  assert.equal(skip(msg()), null);
  assert.equal(skip(msg({ system: true })), 'system');
  assert.equal(skip(msg({ author: { id: 'bot', bot: true } })), 'own-bot');
  assert.equal(skip(msg({ author: { id: 'b2', bot: true } })), 'bot-author');
  assert.equal(skip(msg({ author: { id: 'b2', bot: true } }), board, true), null, 'a tracked message keeps updating');
  assert.equal(skip(msg({ webhookId: '1' })), 'bot-author');
  assert.equal(skip(msg({ author: { id: 'b2', bot: true } }), baseBoard({ ignoreBots: false })), null);
  const old = msg({ id: snowflakeAt(now - 2 * 86_400_000) });
  assert.equal(skip(old, baseBoard({ maxMessageAgeDays: 1 })), 'too-old');
  assert.equal(skip(old, baseBoard({ maxMessageAgeDays: 1 }), true), null);
  assert.equal(skip(old, baseBoard({ maxMessageAgeDays: 3 })), null);
  assert.equal(skip(old, baseBoard({ maxMessageAgeDays: 0 })), null);
  assert.equal(sb.snowflakeTime('175928847299117063'), 1462015105796);
});

test('6. computeCount', () => {
  const u = (id, bot = false) => ({ id, bot });
  const count = (b, lists, extra = {}) => sb.computeCount({ board: baseBoard(b), userLists: lists, authorId: 'a', botUserId: 'bot', ...extra });
  const L = (users, extra = 0) => ({ users, extra });

  assert.equal(count({}, [L([u('1'), u('2'), u('a')])]), 2, 'the author is excluded');
  assert.equal(count({ selfStar: true }, [L([u('1'), u('a')])]), 2);
  assert.equal(count({}, [L([u('1'), u('x', true)])]), 1, 'bots are excluded with ignoreBots');
  assert.equal(count({ ignoreBots: false }, [L([u('1'), u('x', true)])]), 2);
  assert.equal(count({ ignoreBots: false }, [L([u('bot', true), u('1')])]), 1, 'the bot itself is always excluded');
  assert.equal(count({}, [L([u('1'), u('2')]), L([u('2'), u('3')])]), 3, 'a user in two lists counts once');
  assert.equal(count({}, [L([u('1')], 7)]), 8, 'extra is added');
  assert.equal(count({}, [L([u('1')], 2_000_000_000)]), 1_000_000_000, 'clamped');
  assert.equal(count({}, [L(new Collection([['1', u('1')], ['2', u('2')]]))]), 2, 'a Collection works');
  assert.equal(count({}, [L(new Set([u('1')]))]), 1);
  assert.equal(count({}, []), 0);
});

test('7. decideAction (every row of the table, plus blocked entries)', () => {
  const board = baseBoard({ threshold: 3 });
  const noBelow = baseBoard({ threshold: 3, removeBelowThreshold: false });
  const live = (count) => ({ boardMessageId: 'm', count, blocked: false });
  const dropped = (count) => ({ boardMessageId: null, count, blocked: false });
  const blocked = (count) => ({ boardMessageId: null, count, blocked: true });
  const d = (entry, count, b = board) => sb.decideAction({ entry, count, board: b });

  assert.equal(d(null, 3), 'create');
  assert.equal(d(null, 2), 'none');
  assert.equal(d(live(5), 2), 'remove');
  assert.equal(d(live(5), 2, noBelow), 'update');
  assert.equal(d(live(2), 2, noBelow), 'none');
  assert.equal(d(live(3), 4), 'update');
  assert.equal(d(live(3), 3), 'none');
  assert.equal(d(dropped(2), 3), 'repost');
  assert.equal(d(dropped(2), 1), 'store');
  assert.equal(d(dropped(2), 2), 'none');
  // Blocked: never create, repost or update.
  assert.equal(d(blocked(2), 9), 'store');
  assert.equal(d(blocked(9), 9), 'none');
  assert.equal(d(blocked(9), 1), 'store');
});

test('8. buildRepostPayload', () => {
  const message = fakeMsg({ channelId: 'c1' });
  const p = sb.buildRepostPayload({ message, count: 5, emojiText: '⭐' });
  assert.equal(p.content, '⭐ **5** · <#c1>');
  assert.deepEqual(p.allowedMentions, { parse: [] });
  assert.equal(p.components.length, 1);
  const row = p.components[0].toJSON();
  assert.equal(row.components.length, 1);
  assert.equal(row.components[0].url, message.url);
  assert.equal(row.components[0].custom_id, undefined);
  assert.equal(row.components[0].style, 5, 'Link style');
  const e = p.embeds[0].toJSON();
  assert.equal(e.author.name, 'Alice');
  assert.equal(e.description, 'hello');
  assert.equal(e.timestamp, new Date(message.createdTimestamp).toISOString());
  assert.equal(e.title, undefined);
  assert.equal(e.footer, undefined);

  const long = sb.buildRepostPayload({ message: fakeMsg({ content: 'x'.repeat(5000) }), count: 1, emojiText: '⭐' }).embeds[0].toJSON();
  assert.ok(long.description.length <= 4096);

  const att = (id, name, extra = {}) => ({ id, name, url: `https://cdn.example/${name}`, contentType: 'image/png', ...extra });
  const withImages = fakeMsg({ attachments: new Map([['1', att('1', 'SPOILER_x.png')], ['2', att('2', 'pic.png')], ['3', att('3', 'doc.pdf', { contentType: 'application/pdf' })]]) });
  const ej = sb.buildRepostPayload({ message: withImages, count: 1, emojiText: '⭐' }).embeds[0].toJSON();
  assert.equal(ej.image.url, 'https://cdn.example/pic.png', 'the first NON-spoiler image');
  const field = ej.fields.find((f) => f.name === 'Attachments');
  assert.match(field.value, /\|\|\[SPOILER_x\.png\]\(https:\/\/cdn\.example\/SPOILER_x\.png\)\|\|/);
  assert.match(field.value, /doc\.pdf/);
  assert.doesNotMatch(field.value, /pic\.png/, 'the embedded image is not listed again');

  const spoilerOnly = fakeMsg({ attachments: new Map([['1', att('1', 'a.png', { spoiler: true })]]) });
  assert.equal(sb.buildRepostPayload({ message: spoilerOnly, count: 1, emojiText: '⭐' }).embeds[0].toJSON().image, undefined);

  const viaEmbed = fakeMsg({ content: '', embeds: [{ description: 'from embed', title: 't', image: null, thumbnail: null, data: {} }] });
  assert.equal(sb.buildRepostPayload({ message: viaEmbed, count: 1, emojiText: '⭐' }).embeds[0].toJSON().description, 'from embed');
  const gifv = fakeMsg({ embeds: [{ description: null, title: null, image: null, thumbnail: { url: 'https://t.example/g.gif' }, data: { type: 'gifv' } }] });
  assert.equal(sb.selectImage(gifv).url, 'https://t.example/g.gif');
  const stickers = fakeMsg({ content: 'hi', stickers: new Map([['1', { name: 'wave', format: StickerFormatType.PNG, url: 'https://s.example/w.png' }], ['2', { name: 'lottie', format: StickerFormatType.Lottie, url: 'https://s.example/l.json' }]]) });
  const sj = sb.buildRepostPayload({ message: stickers, count: 1, emojiText: '⭐' }).embeds[0].toJSON();
  assert.match(sj.description, /Sticker: \*\*wave\*\*/);
  assert.equal(sj.image.url, 'https://s.example/w.png');

  // Worst case stays under the 6000-character embed total.
  const many = new Map(Array.from({ length: 50 }, (_, i) => [String(i), att(String(i), `file-${'n'.repeat(70)}-${i}.pdf`, { contentType: 'application/pdf' })]));
  const worst = sb.buildRepostPayload({ message: fakeMsg({ content: 'y'.repeat(6000), attachments: many }), count: 1, emojiText: '⭐' });
  const wj = worst.embeds[0].toJSON();
  const total = wj.description.length + wj.author.name.length + wj.fields.reduce((n, f) => n + f.name.length + f.value.length, 0);
  assert.ok(total <= 6000, `embed total ${total}`);
  assert.ok(wj.fields[0].value.length <= 1024);
});

test('9. pickDisplayEmoji', () => {
  const message = fakeMsg();
  const r = (e, count) => ({ emoji: e, count });
  assert.equal(sb.pickDisplayEmoji({ board: baseBoard({ emojis: ['⭐', '🌟'] }), qualifying: [], client: {}, message }), '⭐');
  assert.equal(sb.pickDisplayEmoji({ board: baseBoard({ emojis: [] }), qualifying: [r(STAR, 2), r(GLOW, 5), r(POG, 5)], client: {}, message }), '🌟', 'highest count, ties to the first');
  assert.equal(sb.pickDisplayEmoji({ board: baseBoard({ emojis: [] }), qualifying: [], client: {}, message }), '⭐');
  assert.equal(sb.pickDisplayEmoji({ board: baseBoard({ emojis: [] }), qualifying: [r(POG, 1)], client: {}, message }), '<:pog:123456789012345678>');
});

test('10. createCoalescer, createSemaphore and the burst spacing', async () => {
  const mutex = createKeyedMutex();
  const semaphore = sb.createSemaphore(4);
  const coalescer = sb.createCoalescer({ mutex, semaphore });
  let runs = 0;
  const fn = async () => { runs += 1; await sleep(20); };
  const promises = Array.from({ length: 10 }, () => coalescer.request('m', fn)).filter(Boolean);
  await Promise.all(promises);
  assert.ok(runs >= 1 && runs <= 2, `ran ${runs} times`);
  assert.equal(coalescer.waitingSize(), 0);

  // Running + waiting: a request while one runs is accepted, the next one is coalesced.
  runs = 0;
  const first = coalescer.request('k', fn);
  await sleep(5);
  const second = coalescer.request('k', fn);
  const third = coalescer.request('k', fn);
  assert.ok(first && second);
  assert.equal(third, null);
  await Promise.all([first, second]);
  assert.equal(runs, 2);

  // Different keys run concurrently, up to the semaphore size.
  let active = 0;
  let peak = 0;
  const slow = async () => { active += 1; peak = Math.max(peak, active); await sleep(40); active -= 1; };
  await Promise.all(Array.from({ length: 6 }, (_, i) => coalescer.request(`key${i}`, slow)));
  assert.equal(peak, 4);
  assert.equal(semaphore.active, 0);
  assert.equal(semaphore.queued, 0);

  // A rejecting fn does not wedge the key or the semaphore.
  await assert.rejects(coalescer.request('bad', async () => { throw new Error('boom'); }), /boom/);
  assert.equal(await coalescer.request('bad', async () => 'ok'), 'ok');

  // Spacing: the next run for a key starts at least spacingMs after the previous one ended.
  const spaced = sb.createCoalescer({ mutex, semaphore, spacingMs: 80 });
  const marks = [];
  await spaced.request('s', async () => { marks.push(['start', Date.now()]); });
  const endedAt = Date.now();
  await spaced.request('s', async () => { marks.push(['start', Date.now()]); });
  assert.ok(marks[1][1] - endedAt >= 70, `second run waited ${marks[1][1] - endedAt} ms`);
});

test('11. plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => e.includes('starboard')), []);
  const p = plugins.find((x) => x.id === 'starboard');
  assert.ok(p);
  assert.equal(p.module, 'starboard');
  const raw = pluginMod.plugin;
  assert.equal(raw.id, 'starboard');
  for (const hook of raw.hooks) {
    assert.equal(hook.id, 'starboard');
    assert.ok(HOOK_ORDER[hook.event]?.includes('starboard'), `${hook.event} lists starboard`);
    assert.equal(typeof hook.run, 'function');
    assert.equal(typeof hook.filter, 'function');
  }
  const byEvent = (e) => raw.hooks.find((h) => h.event === e);
  for (const e of ['messageDelete', 'messageDeleteBulk', 'channelDelete']) assert.equal(byEvent(e).always, true, e);
  for (const e of ['messageReactionAdd', 'messageReactionRemove', 'messageReactionRemoveAll', 'messageReactionRemoveEmoji', 'messageUpdate']) {
    assert.ok(byEvent(e), e);
    assert.notEqual(byEvent(e).always, true, `${e} is gated by the module`);
  }
  assert.equal(byEvent('messageDeleteBulk').id, 'starboard');
  assert.deepEqual(raw.components, []);
  assert.deepEqual(raw.jobs, []);
  assert.deepEqual(raw.provides, {});
  // Every HOOK_ORDER slot for starboard is declared.
  for (const [event, ids] of Object.entries(HOOK_ORDER)) {
    if (ids.includes('starboard')) assert.ok(byEvent(event), `declares ${event}`);
  }
});

test('12. command payload', () => {
  const json = buildCommandPayload(cmd);
  assert.equal(json.name, 'starboard');
  assert.deepEqual(json.options.map((o) => o.name), ['create', 'edit', 'delete', 'list']);
  assert.ok(json.options.every((o) => o.type === 1));
  const sub = (n) => json.options.find((o) => o.name === n);
  assert.equal(sub('create').options.length, 11);
  assert.equal(sub('edit').options.length, 15);
  assert.equal(sub('delete').options.length, 1);
  assert.equal(sub('list').options?.length ?? 0, 0);
  assert.deepEqual(sub('create').options.slice(0, 2).map((o) => o.name), ['name', 'channel']);
  assert.deepEqual(sub('create').options.slice(0, 2).map((o) => o.required), [true, true]);
  assert.ok(sub('create').options.slice(2).every((o) => !o.required));
  assert.equal(sub('edit').options[0].name, 'name');
  assert.equal(sub('edit').options[0].autocomplete, true);
  assert.equal(sub('delete').options[0].autocomplete, true);
  assert.ok(sub('edit').options.slice(1).every((o) => !o.required));
  assert.ok(sub('create').options.some((o) => o.name === 'remove_self_stars'));
  assert.ok(sub('edit').options.some((o) => o.name === 'remove_self_stars'));
  const walk = (node) => {
    if (node.description !== undefined) assert.ok(node.description.length >= 1 && node.description.length <= 100, `${node.name}: ${node.description.length}`);
    for (const o of node.options ?? []) walk(o);
  };
  walk(json);
  const chars = (n) => (n.name?.length ?? 0) + (n.description?.length ?? 0) + (n.options ?? []).reduce((s, o) => s + chars(o), 0);
  assert.ok(chars(json) <= 1700, `${chars(json)} characters`);
  assert.equal(json.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.deepEqual(json.contexts, [0]);
  assert.equal(cmd.module, 'starboard');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(typeof cmd.autocomplete, 'function');
  assert.equal(typeof cmd.execute, 'function');
});

test('13. reactionFilter and guildMayHaveBoards', () => {
  const g = G(50);
  sb.__setBoardsMemo(g, null);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '⭐' }), true, 'unknown memo');
  assert.equal(sb.guildMayHaveBoards({ guildId: g }), true);
  assert.equal(sb.reactionFilter({ guildId: null }), false);
  assert.equal(sb.guildMayHaveBoards({ guildId: null }), false);
  sb.__setBoardsMemo(g, []);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '⭐' }), false);
  assert.equal(sb.guildMayHaveBoards({ guildId: g }), false);
  sb.__setBoardsMemo(g, [baseBoard({ emojis: ['⭐'] })]);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '🌟' }), false);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '⭐' }), true);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: undefined }), true, 'RemoveAll has no emoji');
  assert.equal(sb.guildMayHaveBoards({ guildId: g }), true);
  sb.__setBoardsMemo(g, [baseBoard({ enabled: false })]);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '⭐' }), false, 'a disabled board never counts');
  sb.__setBoardsMemo(g, [baseBoard({ enabled: false }), baseBoard({ id: 'b2', emojis: [] })]);
  assert.equal(sb.reactionFilter({ guildId: g, emojiKey: '🌟' }), true, 'an "any" board takes every emoji');
  sb.__setBoardsMemo(g, null);
});

test('14. fetchReactionUsers pages and truncates', async () => {
  const mk = (n, count) => {
    const ids = Array.from({ length: n }, (_, i) => sid('7', i + 1));
    const calls = [];
    return {
      calls,
      count,
      countDetails: { burst: 0 },
      users: {
        fetch: async ({ type, limit, after }) => {
          calls.push({ type, limit, after });
          const page = ids.filter((id) => after === undefined || BigInt(id) > BigInt(after)).slice(0, limit);
          return new Collection(page.map((id) => [id, { id, bot: false }]));
        },
      },
    };
  };
  const small = mk(250, 250);
  const r = await sb.fetchReactionUsers(small);
  assert.equal(r.users.size, 250);
  assert.equal(r.extra, 0);
  assert.equal(small.calls.length, 3);
  assert.equal(small.calls[0].type, ReactionType.Normal);
  assert.equal(small.calls[1].after, sid('7', 100));

  const exact = mk(100, 100);
  assert.equal((await sb.fetchReactionUsers(exact)).users.size, 100);
  assert.equal(exact.calls.length, 2, 'a full page needs one more call to see the end');

  const huge = mk(1200, 1500);
  const h = await sb.fetchReactionUsers(huge);
  assert.equal(h.users.size, sb.MAX_USER_PAGES * 100);
  assert.equal(h.extra, 500, 'the unverified remainder');
  assert.equal(huge.calls.length, sb.MAX_USER_PAGES);

  const burst = { ...mk(3, 5), countDetails: { burst: 2 } };
  await sb.fetchReactionUsers(burst);
  assert.deepEqual(burst.calls.map((c) => c.type), [ReactionType.Normal, ReactionType.Burst], 'super reactions are fetched too');
});

test('15. handleReaction: early exits, the self-react shortcut and coalescing (no database)', async () => {
  const world = makeWorld(51);
  const g = world.guildId;
  const calls = [];
  sb.__setSyncImpl(async (args) => { calls.push(args); await sleep(10); });
  try {
    const board = baseBoard({ channelId: world.boardChannel.id });
    sb.__setBoardsMemo(g, [board]);
    const msg = world.message({ authorId: USER(1) });
    const base = (extra = {}) => world.ctx({
      event: 'messageReactionAdd', message: msg, emojiKey: '⭐', user: { id: USER(2) }, reaction: msg.reactions.cache.get('⭐'),
      ensureMessage: async () => msg, ...extra,
    });

    assert.equal(await sb.handleReaction(base({ emojiKey: '🌟' })), 'skipped', 'no board takes this emoji');
    assert.equal(await sb.handleReaction(base({ user: { id: USER(1) } })), 'self', "the author's own reaction cannot change the count");
    assert.equal(calls.length, 0);

    const inBoardChannel = world.message({ channel: world.boardChannel });
    assert.equal(await sb.handleReaction(base({ message: inBoardChannel })), 'skipped', 'reactions on a board channel never count');

    sb.__setBoardsMemo(g, [baseBoard({ channelId: world.boardChannel.id, enabled: false })]);
    assert.equal(await sb.handleReaction(base()), 'skipped', 'a disabled board');
    sb.__setBoardsMemo(g, [baseBoard({ channelId: world.boardChannel.id, selfStar: true })]);
    const outcomes = await Promise.all([1, 2, 3, 4, 5].map(() => sb.handleReaction(base({ user: { id: USER(1) } }))));
    assert.ok(outcomes.includes('coalesced'), outcomes.join());
    await sleep(60);
    assert.ok(calls.length >= 1 && calls.length <= 2, `${calls.length} syncs for 5 events`);
    assert.equal(calls[0].messageId, msg.id);
    assert.equal(calls[0].channel, world.source);

    // A hook run never throws, whatever the event looks like.
    assert.equal(sb.onReactionEvent({ guildId: g, guild: null, message: null }), undefined);
    await sleep(10);
  } finally {
    sb.__setSyncImpl();
    sb.__setBoardsMemo(g, null);
  }
});

test('16. remove self-stars: the author reaction is removed (Add only, with Manage Messages)', async () => {
  const world = makeWorld(52);
  const g = world.guildId;
  sb.__setSyncImpl(async () => {});
  try {
    sb.__setBoardsMemo(g, [baseBoard({ channelId: world.boardChannel.id, removeSelfStars: true })]);
    const msg = world.message({ authorId: USER(1), reactions: [{ emoji: STAR, userIds: [USER(1)] }] });
    const ctx = (extra = {}) => world.ctx({
      event: 'messageReactionAdd', message: msg, emojiKey: '⭐', user: { id: USER(1) }, reaction: msg.reactions.cache.get('⭐'),
      ensureMessage: async () => msg, ...extra,
    });
    assert.equal(await sb.handleReaction(ctx()), 'self');
    await until(() => world.log.removed.length === 1);
    assert.deepEqual(world.log.removed, [USER(1)]);

    // Somebody else reacting is untouched; so is another emoji.
    await sb.handleReaction(ctx({ user: { id: USER(2) } }));
    await sb.handleReaction(ctx({ emojiKey: '🌟' }));
    await sleep(30);
    assert.equal(world.log.removed.length, 1);

    // A partial message: the author is found through ensureMessage.
    const partial = { ...msg, partial: true, author: undefined, channel: world.source };
    await sb.handleReaction(ctx({ message: partial }));
    await until(() => world.log.removed.length === 2);

    // Without Manage Messages nothing is removed (and nothing throws).
    world.source.permissionsFor = () => ({ has: (flag) => flag !== PermissionFlagsBits.ManageMessages });
    await sb.handleReaction(ctx());
    await sleep(30);
    assert.equal(world.log.removed.length, 2);
    world.source.permissionsFor = allPerms;

    // A Remove event never removes anything.
    await sb.handleReaction(ctx({ event: 'messageReactionRemove' }));
    await sleep(30);
    assert.equal(world.log.removed.length, 2);
  } finally {
    sb.__setSyncImpl();
    sb.__setBoardsMemo(g, null);
  }
});

test('17. RAW_SQL builders are Prisma.Sql without a bare now()', () => {
  assert.equal(sb.RAW_SQL.length, 3);
  for (const { name, build } of sb.RAW_SQL) {
    const q = build();
    assert.equal(typeof q.sql, 'string', name);
    assert.ok(!/(?<![\w.])now\(\)(?!\s*AT\s+TIME\s+ZONE\s+'UTC')/i.test(q.sql), `${name}: ${q.sql}`);
  }
  assert.ok(sb.RAW_SQL.find((r) => r.name === 'starboard.lockBoard').build().sql.includes('FOR UPDATE'));
  assert.match(sb.RAW_SQL.find((r) => r.name === 'starboard.removeIgnoredChannel').build().sql, /AT TIME ZONE 'UTC'/);
});

test('18. settingsSummary', () => {
  const s = sb.settingsSummary(baseBoard({ channelId: 'c9', maxMessageAgeDays: 7, selfStar: false, removeSelfStars: true }), { client: {} });
  assert.equal(s.split('\n').length, 4);
  assert.match(s, /Channel: <#c9> · Emojis: ⭐/);
  assert.match(s, /Self-star: removed/);
  assert.match(s, /Max age: 7 days/);
  assert.doesNotMatch(s, /Reposts/);
  assert.match(sb.settingsSummary(baseBoard(), { reposts: 4 }), /Reposts: 4/);
  assert.match(sb.settingsSummary(baseBoard({ emojis: [], selfStar: true, ignoreBots: false })), /Emojis: any emoji[\s\S]*Self-star: on · Bots: counted/);
});

// ═══════════════════════════ Database cases ═══════════════════════════════════

test('DB: every RAW_SQL statement executes against the migrated schema', { skip: skipDb }, async () => {
  for (const { name, build } of sb.RAW_SQL) {
    if (name === 'starboard.removeIgnoredChannel') assert.equal(await prisma.$executeRaw(build()), 0, name);
    else assert.ok(Array.isArray(await prisma.$queryRaw(build())), name);
  }
  const rows = await prisma.$transaction((tx) => tx.$queryRaw(sb.RAW_SQL[0].build()));
  assert.ok(Array.isArray(rows));
});

test('DB: createBoard enforces the cap, unique names and the defaults', { skip: skipDb }, async () => {
  await setupGuild(1);
  const g = G(1);
  const results = await Promise.all(Array.from({ length: LIMITS.STARBOARDS + 2 }, (_, i) => sb.createBoard(g, { name: `b${i}`, channelId: sid('4', 11) })
    .then(() => 'ok', (err) => err)));
  assert.equal(results.filter((r) => r === 'ok').length, LIMITS.STARBOARDS);
  assert.equal(results.filter((r) => r instanceof sb.BoardLimitError).length, 2);
  assert.equal(await prisma.starboard.count({ where: { guildId: g } }), LIMITS.STARBOARDS);

  // The creates race, so which ten names won is not fixed: work with the boards that exist.
  const [kept, victim] = await prisma.starboard.findMany({ where: { guildId: g }, orderBy: { name: 'asc' }, take: 2, select: { name: true } });
  await assert.rejects(sb.createBoard(g, { name: kept.name, channelId: sid('4', 11) }), (err) => sb.isUniqueViolation(err) || err instanceof sb.BoardLimitError);
  await prisma.starboard.delete({ where: { guildId_name: { guildId: g, name: victim.name } } });
  await assert.rejects(sb.createBoard(g, { name: kept.name, channelId: sid('4', 11) }), (err) => sb.isUniqueViolation(err));

  const row = await sb.getBoardByName(g, kept.name);
  assert.deepEqual(row.emojis, ['⭐']);
  assert.equal(row.threshold, 3);
  assert.equal(row.ignoreBots, true);
  assert.equal(row.removeBelowThreshold, true);
  assert.equal(row.selfStar, false);
  assert.equal(row.removeSelfStars, false);
  assert.equal(row.enabled, true);
  assert.equal(await sb.getBoardByName(g, null), null);
  assert.equal(await sb.getBoardByName(g, 'missing'), null);

  // `undefined` options are dropped so the schema defaults apply (a slot is free again after the delete above).
  const d = await sb.createBoard(g, { name: 'defaults', channelId: sid('4', 11), threshold: undefined, emojis: undefined });
  assert.equal(d.threshold, 3);
  assert.deepEqual(d.emojis, ['⭐']);
  await assert.rejects(sb.createBoard(g, { name: 'one-too-many', channelId: sid('4', 11) }), sb.BoardLimitError, 'the guild is back at the cap');
});

test('DB: getBoards caches, degrades and invalidates', { skip: skipDb }, async () => {
  await setupGuild(2);
  const g = G(2);
  const first = await sb.getBoards(g);
  assert.deepEqual(first, []);
  assert.deepEqual(sb.peekBoards(g), []);
  const board = await mkBoard(2, { name: 'one', emojis: ['⭐', '🌟'], threshold: 2 });
  // createBoard invalidated the memo.
  assert.equal(sb.peekBoards(g), null);
  const boards = await sb.getBoards(g);
  assert.equal(boards.length, 1);
  assert.deepEqual(Object.keys(boards[0]).sort(), [
    'allowNsfw', 'autoReact', 'channelId', 'emojis', 'enabled', 'id', 'ignoreBots', 'ignoredChannelIds', 'maxMessageAgeDays', 'name',
    'removeBelowThreshold', 'removeSelfStars', 'selfStar', 'threshold',
  ]);
  assert.deepEqual(boards[0].emojis, ['⭐', '🌟']);
  assert.equal(sb.peekBoards(g), boards);
  // A direct DB change is not seen until invalidated or fresh.
  await prisma.starboard.update({ where: { id: board.id }, data: { threshold: 9 } });
  assert.equal((await sb.getBoards(g))[0].threshold, 2);
  assert.equal((await sb.getBoards(g, { fresh: true }))[0].threshold, 9);
  await sb.invalidateBoards(g);
  assert.equal(sb.peekBoards(g), null);
  // Postgres unreachable -> null (fail closed). Simulated by a throwing findMany.
  const original = prisma.starboard.findMany;
  prisma.starboard.findMany = async () => { throw new Error('db down'); };
  try {
    assert.equal(await sb.getBoards(g, { fresh: true }), null);
  } finally {
    prisma.starboard.findMany = original;
  }
});

test('DB: a board is created, updated, dropped, reposted and removed as the count moves', { skip: skipDb }, async () => {
  await setupGuild(3);
  const world = makeWorld(3);
  const board = await mkBoard(3, { threshold: 2 });
  const bot = (n) => USER(10 + n);

  // Below the threshold: nothing.
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [bot(1)] }] });
  await world.sync(msg);
  assert.equal(world.log.sent.length, 0);
  assert.equal((await entriesOf(board.id)).length, 0);

  // At the threshold: a repost and an entry.
  msg.reactions.cache.get('⭐').count = 2;
  msg.reactions.cache.set('⭐', { ...msg.reactions.cache.get('⭐'), users: world.message({ reactions: [{ emoji: STAR, userIds: [bot(1), bot(2)] }] }).reactions.cache.get('⭐').users });
  await world.sync(msg);
  assert.equal(world.log.sent.length, 1);
  const sent = world.log.sent[0];
  assert.equal(sent.payload.content, `⭐ **2** · <#${world.source.id}>`);
  assert.deepEqual(sent.payload.allowedMentions, { parse: [] });
  let [entry] = await entriesOf(board.id);
  assert.equal(entry.boardMessageId, sent.id);
  assert.equal(entry.count, 2);
  assert.equal(entry.authorId, USER(1));
  assert.equal(entry.sourceMessageId, msg.id);
  assert.equal(entry.blocked, false);

  // Unchanged count: no edit. A higher count edits only the content line.
  await world.sync(msg);
  assert.equal(world.log.edits.length, 0);
  const more = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [bot(1), bot(2), bot(3), bot(4)] }] });
  world.source.messages.cache.set(msg.id, more);
  await world.sync(more);
  assert.equal(world.log.edits.length, 1);
  assert.equal(world.log.edits[0].id, sent.id);
  assert.deepEqual(world.log.edits[0].payload, { content: `⭐ **4** · <#${world.source.id}>`, allowedMentions: { parse: [] } });
  [entry] = await entriesOf(board.id);
  assert.equal(entry.count, 4);

  // Below the threshold: the repost is deleted, the entry stays as dropped.
  const fewer = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [bot(1)] }] });
  await world.sync(fewer);
  assert.deepEqual(world.log.deletes, [sent.id]);
  [entry] = await entriesOf(board.id);
  assert.equal(entry.boardMessageId, null);
  assert.equal(entry.count, 1);
  assert.equal(entry.blocked, false);

  // Dropped + still below: the count is stored; dropped + unchanged: nothing.
  const none = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [bot(1), bot(2)].slice(0, 1) }] });
  await world.sync(none);
  assert.equal(world.log.sent.length, 1);

  // Back at the threshold: reposted into the same entry.
  const back = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [bot(1), bot(2), bot(3)] }] });
  await world.sync(back);
  assert.equal(world.log.sent.length, 2);
  const entries = await entriesOf(board.id);
  assert.equal(entries.length, 1);
  assert.equal(entries[0].boardMessageId, world.log.sent[1].id);
  assert.equal(entries[0].count, 3);
});

test('DB: removeBelowThreshold off keeps the repost; self-stars, bots and the bot itself are not counted', { skip: skipDb }, async () => {
  await setupGuild(4);
  const world = makeWorld(4);
  const board = await mkBoard(4, { threshold: 2, removeBelowThreshold: false });

  // The author and a bot do not count (selfStar false, ignoreBots true): 2 real users out of 4 reactors.
  const msg = world.message({ authorId: USER(1), reactions: [{ emoji: STAR, userIds: [USER(1), USER(2), USER(3), USER(4)], bots: [USER(4)] }] });
  await world.sync(msg);
  let [entry] = await entriesOf(board.id);
  assert.equal(entry.count, 2);
  assert.equal(world.log.sent.length, 1);

  // Falls to 1: kept (removeBelowThreshold off), edited.
  const lower = world.message({ id: msg.id, authorId: USER(1), reactions: [{ emoji: STAR, userIds: [USER(1), USER(2)] }] });
  await world.sync(lower);
  [entry] = await entriesOf(board.id);
  assert.equal(entry.count, 1);
  assert.equal(entry.boardMessageId, world.log.sent[0].id);
  assert.equal(world.log.edits.length, 1);
  assert.equal(world.log.deletes.length, 0);

  // selfStar on counts the author; the bot's own reaction never counts.
  await prisma.starboard.update({ where: { id: board.id }, data: { selfStar: true, ignoreBots: false } });
  await sb.invalidateBoards(G(4));
  const again = world.message({ id: msg.id, authorId: USER(1), reactions: [{ emoji: STAR, userIds: [USER(1), USER(2), BOT], me: true }] });
  await world.sync(again);
  [entry] = await entriesOf(board.id);
  assert.equal(entry.count, 2, 'author counted, the bot not (fast path: count - me)');
});

test('DB: an "any" board counts distinct users over every emoji', { skip: skipDb }, async () => {
  await setupGuild(5);
  const world = makeWorld(5);
  const board = await mkBoard(5, { threshold: 3, emojis: [] });
  const msg = world.message({ reactions: [
    { emoji: STAR, userIds: [USER(2), USER(3)] },
    { emoji: GLOW, userIds: [USER(3), USER(4)] },
  ] });
  await world.sync(msg);
  const [entry] = await entriesOf(board.id);
  assert.equal(entry.count, 3, '3 distinct users');
  assert.equal(world.log.sent[0].payload.content.startsWith('⭐ **3**'), true, 'the highest-count emoji, ties to the first');
  // Each emoji was fetched once for this run.
  assert.equal(msg.reactions.cache.get('⭐').fetches.length, 1);
});

test('DB: boards are independent; a message on two boards is fetched once per emoji', { skip: skipDb }, async () => {
  await setupGuild(6);
  const world = makeWorld(6);
  const second = nextId();
  const secondChannel = { ...world.boardChannel, id: second, send: world.boardChannel.send };
  world.guild.channels.cache.set(second, secondChannel);
  const a = await sb.createBoard(G(6), { name: 'a', channelId: world.boardChannel.id, threshold: 2 });
  const b = await sb.createBoard(G(6), { name: 'b', channelId: second, threshold: 3 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] });
  await world.sync(msg);
  assert.equal((await entriesOf(a.id)).length, 1);
  assert.equal((await entriesOf(b.id)).length, 0, 'the second board needs 3');
  assert.equal(msg.reactions.cache.get('⭐').fetches.length, 1);
});

test('DB: eligibility filters (ignored, board channel, NSFW, bot author, age)', { skip: skipDb }, async () => {
  await setupGuild(7);
  const world = makeWorld(7);
  const board = await mkBoard(7, { threshold: 1, maxMessageAgeDays: 2, ignoredChannelIds: [] });
  const users = [USER(2), USER(3)];
  const run = async (msgOpts, channel = world.source) => {
    const msg = world.message({ reactions: [{ emoji: STAR, userIds: users }], channel, ...msgOpts });
    await world.sync(msg, channel);
    return msg;
  };

  await run({ authorId: USER(9), bot: true });
  assert.equal(world.log.sent.length, 0, 'bot author');
  await run({ id: snowflakeAt(Date.now() - 3 * 86_400_000) });
  assert.equal(world.log.sent.length, 0, 'too old (no fetch needed)');
  await run({}, world.boardChannel);
  assert.equal(world.log.sent.length, 0, 'a message inside the board channel');

  await prisma.starboard.update({ where: { id: board.id }, data: { ignoredChannelIds: [world.source.id] } });
  await sb.invalidateBoards(G(7));
  await run({});
  assert.equal(world.log.sent.length, 0, 'ignored channel');
  await prisma.starboard.update({ where: { id: board.id }, data: { ignoredChannelIds: [] } });
  await sb.invalidateBoards(G(7));

  world.source.nsfw = true;
  await run({});
  assert.equal(world.log.sent.length, 0, 'NSFW source, board not allowed');
  world.source.nsfw = false;

  await run({});
  assert.equal(world.log.sent.length, 1, 'a normal message passes');

  // A tracked message keeps updating after it ages past the limit.
  const [entry] = await entriesOf(board.id);
  const old = world.message({ id: entry.sourceMessageId, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3), USER(4)] }] });
  old.createdTimestamp = Date.now() - 30 * 86_400_000;
  await world.sync(old);
  assert.equal((await entriesOf(board.id))[0].count, 3);

  // Disabled boards are skipped.
  await prisma.starboard.update({ where: { id: board.id }, data: { enabled: false } });
  await sb.invalidateBoards(G(7));
  const before = world.log.sent.length;
  await run({});
  assert.equal(world.log.sent.length, before);
});

test('DB: permissions, a missing channel and send errors', { skip: skipDb }, async () => {
  await setupGuild(8);
  const world = makeWorld(8);
  const board = await mkBoard(8, { threshold: 1 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });

  // Missing Send Messages: no entry, no post; the next event retries.
  world.boardChannel.permissionsFor = () => ({ has: (flag) => flag !== PermissionFlagsBits.SendMessages });
  await world.sync(msg);
  assert.equal(world.log.sent.length, 0);
  assert.equal((await entriesOf(board.id)).length, 0);
  world.boardChannel.permissionsFor = allPerms;

  // 50013 from send: same.
  world.sendError = discordErr(50013);
  await world.sync(msg);
  assert.equal((await entriesOf(board.id)).length, 0);
  world.sendError = null;

  // Another send error propagates after the other boards were served.
  world.sendError = new Error('boom');
  await assert.rejects(world.sync(msg), /boom/);
  world.sendError = null;

  await world.sync(msg);
  assert.equal(world.log.sent.length, 1);

  // The board channel vanished: REST says 10003 -> the board is disabled and its entries dropped.
  world.guild.channels.cache.delete(world.boardChannel.id);
  const more = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] });
  await world.sync(more);
  const row = await prisma.starboard.findUnique({ where: { id: board.id } });
  assert.equal(row.enabled, false);
  assert.equal((await entriesOf(board.id))[0].boardMessageId, null);

  // An unavailable guild never disables anything (rule 20).
  await prisma.starboard.update({ where: { id: board.id }, data: { enabled: true } });
  await sb.invalidateBoards(G(8));
  world.guild.available = false;
  await world.sync(world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] }));
  assert.equal((await prisma.starboard.findUnique({ where: { id: board.id } })).enabled, true);
});

test('DB: a repost deleted by staff is blocked and never reposted; the bot own deletions are not', { skip: skipDb }, async () => {
  await setupGuild(9);
  const world = makeWorld(9);
  const board = await mkBoard(9, { threshold: 1 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  await world.sync(msg);
  const [entry] = await entriesOf(board.id);
  const repostId = entry.boardMessageId;

  // The bot's own deletion (marked) changes nothing.
  markBotDeletion(repostId, 'starboard');
  sb.onMessageDelete(world.ctx({ message: { id: repostId, channelId: world.boardChannel.id } }));
  await sleep(150);
  assert.equal((await entriesOf(board.id))[0].boardMessageId, repostId);

  // Staff delete it: the entry is blocked.
  markBotDeletion(repostId, 'other'); // a different reason is still processed
  sb.onMessageDelete(world.ctx({ message: { id: repostId, channelId: world.boardChannel.id } }));
  const blocked = await until(async () => { const [e] = await entriesOf(board.id); return e.blocked ? e : null; });
  assert.equal(blocked.boardMessageId, null);

  // More reactions: no repost, nothing created, nothing edited.
  const more = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3), USER(4)] }] });
  await world.sync(more);
  assert.equal(world.log.sent.length, 1);
  assert.equal(world.log.edits.length, 0);

  // Deleting the source still removes the (blocked) entry.
  sb.onMessageDelete(world.ctx({ message: { id: msg.id, channelId: world.source.id } }));
  await until(async () => (await entriesOf(board.id)).length === 0);
});

test('DB: a repost deleted by hand (10008 on edit) is blocked', { skip: skipDb }, async () => {
  await setupGuild(10);
  const world = makeWorld(10);
  const board = await mkBoard(10, { threshold: 1 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  await world.sync(msg);

  world.editError = discordErr(10008);
  const more = world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] });
  await world.sync(more);
  const [entry] = await entriesOf(board.id);
  assert.equal(entry.blocked, true);
  assert.equal(entry.boardMessageId, null);
  world.editError = null;
  await world.sync(world.message({ id: msg.id, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3), USER(4)] }] }));
  assert.equal(world.log.sent.length, 1, 'never reposted');

  // A 50013 on edit only warns: the count is still stored.
  const world2 = makeWorld(11);
  await setupGuild(11);
  const board2 = await mkBoard(11, { threshold: 1 });
  const m2 = world2.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  await world2.sync(m2);
  world2.editError = discordErr(50013);
  await world2.sync(world2.message({ id: m2.id, reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] }));
  const [e2] = await entriesOf(board2.id);
  assert.equal(e2.count, 2);
  assert.ok(e2.boardMessageId);

  // 10008 when removing: the entry is dropped (not blocked).
  const world3 = makeWorld(12);
  await setupGuild(12);
  const board3 = await mkBoard(12, { threshold: 2 });
  const m3 = world3.message({ reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] });
  await world3.sync(m3);
  world3.deleteError = discordErr(10008);
  await world3.sync(world3.message({ id: m3.id, reactions: [{ emoji: STAR, userIds: [USER(2)] }] }));
  const [e3] = await entriesOf(board3.id);
  assert.equal(e3.boardMessageId, null);
  assert.equal(e3.blocked, false);
  // 50013 when removing: the entry stays live, the count is stored.
  const world4 = makeWorld(13);
  await setupGuild(13);
  const board4 = await mkBoard(13, { threshold: 2 });
  const m4 = world4.message({ reactions: [{ emoji: STAR, userIds: [USER(2), USER(3)] }] });
  await world4.sync(m4);
  world4.deleteError = discordErr(50013);
  await world4.sync(world4.message({ id: m4.id, reactions: [{ emoji: STAR, userIds: [USER(2)] }] }));
  const [e4] = await entriesOf(board4.id);
  assert.ok(e4.boardMessageId);
  assert.equal(e4.count, 1);
});

test('DB: autoReact reacts with the first emoji; a failing react never undoes the repost', { skip: skipDb }, async () => {
  await setupGuild(14);
  const world = makeWorld(14);
  const board = await mkBoard(14, { threshold: 1, autoReact: true, emojis: ['🌟', '⭐'] });
  await world.sync(world.message({ reactions: [{ emoji: GLOW, userIds: [USER(2)] }] }));
  assert.deepEqual(world.log.reacts, ['🌟']);

  const world2 = makeWorld(15);
  await setupGuild(15);
  const board2 = await mkBoard(15, { threshold: 1, autoReact: true, emojis: ['123456789012345678'] });
  world2.boardChannel.send = async (payload) => {
    const m = { id: nextId(), payload, delete: async () => {}, react: async () => { throw discordErr(10014); } };
    world2.log.sent.push(m);
    return m;
  };
  await world2.sync(world2.message({ reactions: [{ emoji: POG, userIds: [USER(2)] }] }));
  assert.equal(world2.log.sent.length, 1);
  assert.equal((await entriesOf(board2.id)).length, 1);
  assert.ok(board);
});

test('DB: source edits are mirrored onto the live repost', { skip: skipDb }, async () => {
  await setupGuild(16);
  const world = makeWorld(16);
  const board = await mkBoard(16, { threshold: 1 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }], content: 'first' });
  await world.sync(msg);

  const edited = world.message({ id: msg.id, content: 'second', reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  sb.onMessageUpdate(world.ctx({ newMessage: edited, message: edited }));
  await until(() => world.log.edits.length === 1);
  const { id, payload } = world.log.edits[0];
  assert.equal(id, (await entriesOf(board.id))[0].boardMessageId);
  assert.equal(payload.embeds[0].toJSON().description, 'second');
  assert.deepEqual(payload.allowedMentions, { parse: [] });
  assert.equal(payload.content, `⭐ **1** · <#${world.source.id}>`);

  // 10008 while mirroring blocks the entry.
  world.editError = discordErr(10008);
  sb.onMessageUpdate(world.ctx({ newMessage: edited, message: edited }));
  await until(async () => (await entriesOf(board.id))[0].blocked);

  // A message without an entry or a guild without boards is a no-op.
  sb.onMessageUpdate(world.ctx({ newMessage: world.message({ content: 'x' }) }));
  sb.onMessageUpdate({ guildId: G(16) });
  await sleep(50);
});

test('DB: deleting the source deletes its reposts; bulk delete handles reposts and sources', { skip: skipDb }, async () => {
  await setupGuild(17);
  const world = makeWorld(17);
  const board = await mkBoard(17, { threshold: 1 });
  const a = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  const b = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  const c = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  for (const m of [a, b, c]) await world.sync(m);
  const entries = await entriesOf(board.id);
  assert.equal(entries.length, 3);
  const repostOf = (m) => entries.find((e) => e.sourceMessageId === m.id).boardMessageId;

  // Source deleted: the repost goes first, then the entry. A repost the bot cannot delete is reported, the entry still goes.
  sb.onMessageDelete(world.ctx({ message: { id: a.id, channelId: world.source.id, partial: true } }));
  await until(async () => (await entriesOf(board.id)).length === 2);
  assert.ok(world.log.deletes.includes(repostOf(a)));

  // Bulk: a purge of sources.
  const before = world.log.deletes.length;
  sb.onMessageDeleteBulk(world.ctx({ channel: world.source, messages: new Collection([[b.id, { id: b.id }], ['999', { id: '999' }]]) }));
  await until(async () => (await entriesOf(board.id)).length === 1);
  assert.ok(world.log.deletes.slice(before).includes(repostOf(b)));

  // Bulk: a purge in the board channel blocks the reposts (except the bot's own deletions).
  const cRepost = repostOf(c);
  sb.onMessageDeleteBulk(world.ctx({ channel: world.boardChannel, messages: new Collection([[cRepost, { id: cRepost }]]) }));
  const blocked = await until(async () => { const [e] = await entriesOf(board.id); return e.blocked ? e : null; });
  assert.equal(blocked.boardMessageId, null);

  // A guild without boards: the hooks do nothing and never throw.
  await sb.invalidateBoards(G(17));
  sb.onMessageDelete({ guildId: G(17), message: { id: '1', channelId: '2' }, guild: world.guild, client: world.client });
  sb.onMessageDeleteBulk({ guildId: G(17), messages: new Collection(), channel: world.source, guild: world.guild, client: world.client });
  await sleep(50);
});

test('DB: deleting a board channel disables its boards, cleans the ignore lists and drops source entries', { skip: skipDb }, async () => {
  await setupGuild(18);
  const world = makeWorld(18);
  const doomed = sid('4', 998);
  const keep = await mkBoard(18, { name: 'keep', ignoredChannelIds: [doomed, 'other-chan'], threshold: 1 });
  const gone = await sb.createBoard(G(18), { name: 'gone', channelId: doomed, threshold: 1 });
  const msg = world.message({ reactions: [{ emoji: STAR, userIds: [USER(2)] }] });
  await world.sync(msg);
  await prisma.starboardEntry.create({ data: { starboardId: gone.id, sourceChannelId: world.source.id, sourceMessageId: '777', authorId: USER(1), boardMessageId: 'live', count: 2 } });
  const stamp = (await prisma.starboard.findUnique({ where: { id: keep.id } })).updatedAt;
  await sleep(20);

  await sb.onChannelDelete(world.ctx({ channel: { id: doomed } }));
  const goneRow = await prisma.starboard.findUnique({ where: { id: gone.id } });
  assert.equal(goneRow.enabled, false);
  assert.equal((await entriesOf(gone.id))[0].boardMessageId, null);
  const keepRow = await prisma.starboard.findUnique({ where: { id: keep.id } });
  assert.deepEqual(keepRow.ignoredChannelIds, ['other-chan']);
  assert.ok(keepRow.updatedAt > stamp, 'the raw UPDATE sets updatedAt');
  assert.equal(keepRow.enabled, true);
  assert.equal(sb.peekBoards(G(18)), null, 'the cache was invalidated');

  // A deleted SOURCE channel drops its entries; the reposts stay as an archive.
  await sb.onChannelDelete(world.ctx({ channel: { id: world.source.id } }));
  assert.equal((await entriesOf(keep.id)).length, 0);
  assert.equal((await entriesOf(gone.id)).length, 0);
  assert.equal(world.log.deletes.length, 0);

  // Nothing matches: a no-op that never throws.
  await sb.onChannelDelete(world.ctx({ channel: { id: 'unrelated' } }));
  await sb.onChannelDelete({ guildId: G(18), channel: null });
});

test('DB: updateBoard (channel change drops entries, ignore list, limits, conflicts), deleteBoard, list and search', { skip: skipDb }, async () => {
  await setupGuild(19);
  const g = G(19);
  const board = await mkBoard(19, { name: 'alpha', threshold: 1 });
  const beta = await sb.createBoard(g, { name: 'beta', channelId: sid('4', 192) });
  await assert.rejects(sb.createBoard(g, { name: 'beta', channelId: sid('4', 192) }), (err) => sb.isUniqueViolation(err), 'same name in the same guild is refused');
  await prisma.starboardEntry.createMany({ data: [
    { starboardId: board.id, sourceChannelId: 'c', sourceMessageId: 's1', authorId: 'a', boardMessageId: 'r1', count: 3 },
    { starboardId: board.id, sourceChannelId: 'c', sourceMessageId: 's2', authorId: 'a', boardMessageId: null, count: 1 },
    { starboardId: beta.id, sourceChannelId: 'c', sourceMessageId: 's1', authorId: 'a', boardMessageId: 'r2', count: 3 },
  ] });

  // Plain change keeps entries.
  const r1 = await sb.updateBoard(g, board.id, { threshold: 5, selfStar: true });
  assert.equal(r1.before.threshold, 1);
  assert.equal(r1.after.threshold, 5);
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: board.id, boardMessageId: { not: null } } }), 1);

  // Channel change drops this board's live entries only.
  const newChannel = sid('4', 193);
  const r2 = await sb.updateBoard(g, board.id, { channelId: newChannel });
  assert.equal(r2.after.channelId, newChannel);
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: board.id, boardMessageId: { not: null } } }), 0);
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: beta.id, boardMessageId: { not: null } } }), 1);
  // The same channel again does not drop anything.
  await prisma.starboardEntry.update({ where: { starboardId_sourceMessageId: { starboardId: board.id, sourceMessageId: 's1' } }, data: { boardMessageId: 'r9' } });
  await sb.updateBoard(g, board.id, { channelId: newChannel, threshold: 6 });
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: board.id, boardMessageId: { not: null } } }), 1);

  // Ignore list: add (idempotent), remove, cap.
  await sb.updateBoard(g, board.id, { addIgnored: 'x1' });
  await sb.updateBoard(g, board.id, { addIgnored: 'x1' });
  assert.deepEqual((await sb.getBoardByName(g, 'alpha')).ignoredChannelIds, ['x1']);
  await sb.updateBoard(g, board.id, { removeIgnored: 'x1' });
  assert.deepEqual((await sb.getBoardByName(g, 'alpha')).ignoredChannelIds, []);
  await prisma.starboard.update({ where: { id: board.id }, data: { ignoredChannelIds: Array.from({ length: sb.MAX_IGNORED_CHANNELS }, (_, i) => `i${i}`) } });
  await assert.rejects(sb.updateBoard(g, board.id, { addIgnored: 'one-more' }), sb.IgnoreLimitError);
  await sb.updateBoard(g, board.id, { addIgnored: 'i3' }); // already there: not a new entry

  // Conflicts and errors.
  await assert.rejects(sb.updateBoard(g, board.id, { removeSelfStars: true }), sb.SettingsConflictError);
  await sb.updateBoard(g, board.id, { selfStar: false, removeSelfStars: true });
  await assert.rejects(sb.updateBoard(g, board.id, { selfStar: true }), sb.SettingsConflictError);
  await assert.rejects(sb.updateBoard(g, 'missing', { threshold: 2 }), sb.NotFoundError);
  await assert.rejects(sb.updateBoard('0', board.id, { threshold: 2 }), sb.NotFoundError, 'another guild cannot edit it');
  await assert.rejects(sb.updateBoard(g, board.id, { name: 'beta' }), (err) => sb.isUniqueViolation(err));
  const renamed = await sb.updateBoard(g, board.id, { name: 'gamma', enabled: false });
  assert.equal(renamed.after.name, 'gamma');
  assert.equal(renamed.after.enabled, false);

  // list + search
  const list = await sb.listBoardsWithCounts(g);
  assert.deepEqual(list.map((b) => b.name), ['gamma', 'beta']);
  assert.deepEqual(list.map((b) => b.reposts), [1, 1]);
  assert.deepEqual((await sb.listBoardsWithCounts('0')), []);
  assert.deepEqual((await sb.searchBoards(g, 'GAM')).map((b) => b.name), ['gamma']);
  assert.deepEqual((await sb.searchBoards(g, '')).map((b) => b.name), ['beta', 'gamma']);
  assert.deepEqual(await sb.searchBoards(g, 'zzz'), []);

  // deleteBoard cascades to entries and is scoped to the guild.
  assert.equal(await sb.deleteBoard('0', board.id), 0);
  assert.equal(await sb.deleteBoard(g, board.id), 1);
  assert.equal(await sb.deleteBoard(g, board.id), 0);
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: board.id } }), 0);
  assert.equal(await prisma.starboardEntry.count({ where: { starboardId: beta.id } }), 1);
});

test('DB: the entry unique key, relation-filter lookups and blocked default', { skip: skipDb }, async () => {
  await setupGuild(20);
  const board = await mkBoard(20);
  const data = { starboardId: board.id, sourceChannelId: 'c', sourceMessageId: 's', authorId: 'a', boardMessageId: 'r', count: 1 };
  const e = await prisma.starboardEntry.create({ data });
  assert.equal(e.blocked, false);
  await assert.rejects(prisma.starboardEntry.create({ data }), (err) => sb.isUniqueViolation(err));
  const found = await prisma.starboardEntry.findFirst({ where: { boardMessageId: 'r', starboard: { guildId: G(20) } } });
  assert.equal(found.id, e.id);
  assert.equal(await prisma.starboardEntry.findFirst({ where: { boardMessageId: 'r', starboard: { guildId: G(21) } } }), null);
  const r = await prisma.starboardEntry.updateMany({ where: { boardMessageId: { in: ['r', 'q'] }, starboard: { guildId: G(20) } }, data: { boardMessageId: null, blocked: true } });
  assert.equal(r.count, 1);
  const bulk = await prisma.starboardEntry.findMany({ where: { sourceMessageId: { in: ['s', 's'] }, starboard: { guildId: G(20) } }, select: { sourceMessageId: true }, distinct: ['sourceMessageId'] });
  assert.equal(bulk.length, 1);
});

// ── /starboard against fake interactions ──────────────────────────────────────

function makeInteraction(world, { sub, strings = {}, ints = {}, bools = {}, channels = {}, focused = '' }) {
  const given = [...Object.keys(strings), ...Object.keys(ints), ...Object.keys(bools), ...Object.keys(channels)];
  const replies = [];
  const i = {
    guild: world.guild, guildId: world.guildId, user: { id: USER(1) }, createdTimestamp: Date.now(),
    deferred: false, replied: false, ephemeral: null, replies, responded: null,
    options: {
      getSubcommand: () => sub,
      getString: (n) => strings[n] ?? null,
      getInteger: (n) => ints[n] ?? null,
      getBoolean: (n) => bools[n] ?? null,
      getChannel: (n) => channels[n] ?? null,
      getFocused: () => focused,
      data: [{ name: sub, options: given.map((name) => ({ name })) }],
    },
    async deferReply({ ephemeral } = {}) { i.deferred = true; i.ephemeral = !!ephemeral; },
    async editReply(p) { i.replied = true; replies.push({ via: 'edit', ...p }); return { id: 'm' }; },
    async followUp(p) { i.replied = true; replies.push({ via: 'followUp', ...p }); return { id: 'm' }; },
    async respond(choices) { i.responded = choices; },
  };
  return i;
}
const reply0 = (i) => i.replies[0].embeds[0].toJSON();
const titleOf = (i) => reply0(i).title;

test('DB: /starboard create validates, creates and explains', { skip: skipDb }, async () => {
  await setupGuild(30);
  const world = makeWorld(30);
  const run = async (opts) => { const i = makeInteraction(world, { sub: 'create', ...opts }); await cmd.execute(i, world.client); return i; };

  let i = await run({ strings: { name: '***' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Invalid Name/);
  assert.equal(i.deferred, true);
  assert.equal(i.ephemeral, false, 'public reply');

  i = await run({ strings: { name: 'ok', emoji: 'abc' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Invalid Emoji/);

  i = await run({ strings: { name: 'ok' }, bools: { allow_nsfw: true }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Channel Not Age-Restricted/);

  i = await run({ strings: { name: 'ok' }, bools: { self_star: true, remove_self_stars: true }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Invalid Options/);
  assert.match(reply0(i).description, /cannot also be removed/);

  world.boardChannel.permissionsFor = () => ({ has: (flag) => flag !== PermissionFlagsBits.EmbedLinks });
  i = await run({ strings: { name: 'ok' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Missing Permissions/);
  assert.match(reply0(i).description, /Embed Links/);
  world.boardChannel.permissionsFor = (() => ({ has: (flag) => flag !== PermissionFlagsBits.AddReactions }));
  i = await run({ strings: { name: 'ok' }, bools: { auto_react: true }, channels: { channel: world.boardChannel } });
  assert.match(reply0(i).description, /Add Reactions/);
  world.boardChannel.permissionsFor = allPerms;
  assert.equal(await prisma.starboard.count({ where: { guildId: world.guildId } }), 0, 'nothing was created by the failures');

  i = await run({ strings: { name: ' Hall Of Fame ', emoji: '⭐ 🌟' }, ints: { threshold: 2 }, bools: { ignore_bots: false }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Starboard Created/);
  assert.match(reply0(i).description, /\*\*hall-of-fame\*\* reposts messages with \*\*2\+\*\* ⭐ 🌟 reactions to <#/);
  assert.match(reply0(i).description, /Bots: counted/);
  const row = await sb.getBoardByName(world.guildId, 'hall-of-fame');
  assert.deepEqual(row.emojis, ['⭐', '🌟']);
  assert.equal(row.threshold, 2);
  assert.equal(row.ignoreBots, false);
  assert.equal(row.channelId, world.boardChannel.id);
  assert.doesNotMatch(reply0(i).description, /module is off/);

  i = await run({ strings: { name: 'HALL OF FAME' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Name Taken/);

  i = await run({ strings: { name: 'any-board', emoji: 'any' }, bools: { remove_self_stars: true }, channels: { channel: world.boardChannel } });
  assert.match(reply0(i).description, /any emoji/);
  assert.equal((await sb.getBoardByName(world.guildId, 'any-board')).removeSelfStars, true);

  // Module off: the warning is appended; the command still works.
  await updateSettings(world.guildId, { starboardEnabled: false });
  i = await run({ strings: { name: 'third' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Starboard Created/);
  assert.ok(reply0(i).description.endsWith('Turn it on with `/module enable starboard`.'));
  await updateSettings(world.guildId, { starboardEnabled: true });

  // The cap.
  for (let n = 0; n < LIMITS.STARBOARDS; n += 1) await sb.createBoard(world.guildId, { name: `fill${n}`, channelId: world.boardChannel.id }).catch(() => {});
  i = await run({ strings: { name: 'overflow' }, channels: { channel: world.boardChannel } });
  assert.match(titleOf(i), /Limit Reached/);
  assert.match(reply0(i).description, /at most 10 starboards/);
});

test('DB: /starboard edit changes, reports and validates', { skip: skipDb }, async () => {
  await setupGuild(31);
  const world = makeWorld(31);
  const board = await mkBoard(31, { name: 'edit-me', threshold: 3 });
  const run = async (opts) => { const i = makeInteraction(world, { sub: 'edit', ...opts }); await cmd.execute(i, world.client); return i; };
  const ignored = { id: sid('4', 310), toString() { return '<#ignored>'; } };

  let i = await run({ strings: { name: 'nope' } });
  assert.match(titleOf(i), /Not Found/);
  i = await run({ strings: { name: 'edit-me' } });
  assert.match(titleOf(i), /Nothing To Change/);
  i = await run({ strings: { name: 'edit-me', rename: '!!!' } });
  assert.match(titleOf(i), /Invalid Name/);
  i = await run({ strings: { name: 'edit-me', emoji: 'zzz' } });
  assert.match(titleOf(i), /Invalid Emoji/);
  i = await run({ strings: { name: 'edit-me' }, channels: { ignore_channel: ignored, unignore_channel: ignored } });
  assert.match(titleOf(i), /Invalid Options/);
  i = await run({ strings: { name: 'edit-me' }, bools: { allow_nsfw: true } });
  assert.match(titleOf(i), /Channel Not Age-Restricted/);
  i = await run({ strings: { name: 'edit-me' }, bools: { remove_self_stars: true, self_star: true } });
  assert.match(titleOf(i), /Invalid Options/);

  i = await run({ strings: { name: 'edit-me', emoji: '🌟' }, ints: { threshold: 5 }, bools: { remove_below: false } });
  assert.match(titleOf(i), /Starboard Updated/);
  const desc = reply0(i).description;
  assert.match(desc, /• \*\*Threshold:\*\* 3 → 5/);
  assert.match(desc, /• \*\*Emojis:\*\* ⭐ → 🌟/);
  assert.match(desc, /• \*\*Below threshold:\*\* remove → keep/);
  assert.doesNotMatch(desc, /Self-star:\*\*/);
  assert.match(desc, /Threshold: 5/);

  // remove_self_stars alone is fine; combined with an existing self_star it is refused (effective values).
  i = await run({ strings: { name: 'edit-me' }, bools: { remove_self_stars: true } });
  assert.match(reply0(i).description, /Remove self-stars:\*\* off → on/);
  i = await run({ strings: { name: 'edit-me' }, bools: { self_star: true } });
  assert.match(titleOf(i), /Invalid Options/);

  // Ignore list.
  i = await run({ strings: { name: 'edit-me' }, channels: { ignore_channel: ignored } });
  assert.match(reply0(i).description, /\+ <#ignored> ignored/);
  i = await run({ strings: { name: 'edit-me' }, channels: { ignore_channel: ignored } });
  assert.match(reply0(i).description, /<#ignored> was already ignored/);
  i = await run({ strings: { name: 'edit-me' }, channels: { unignore_channel: ignored } });
  assert.match(reply0(i).description, /− <#ignored> no longer ignored/);
  i = await run({ strings: { name: 'edit-me' }, channels: { unignore_channel: ignored } });
  assert.match(reply0(i).description, /<#ignored> was not ignored/);
  await prisma.starboard.update({ where: { id: board.id }, data: { ignoredChannelIds: Array.from({ length: sb.MAX_IGNORED_CHANNELS }, (_, n) => `i${n}`) } });
  i = await run({ strings: { name: 'edit-me' }, channels: { ignore_channel: { id: 'fresh' } } });
  assert.match(titleOf(i), /Limit Reached/);
  await prisma.starboard.update({ where: { id: board.id }, data: { ignoredChannelIds: [] } });

  // Rename + off + channel change.
  await prisma.starboardEntry.create({ data: { starboardId: board.id, sourceChannelId: 'c', sourceMessageId: 's', authorId: 'a', boardMessageId: 'live', count: 3 } });
  const newBoardChannel = { ...world.boardChannel, id: sid('4', 319), nsfw: false, toString() { return `<#${sid('4', 319)}>`; } };
  world.guild.channels.cache.set(newBoardChannel.id, newBoardChannel);
  i = await run({ strings: { name: 'edit-me', rename: 'Renamed' }, bools: { enabled: false }, channels: { channel: newBoardChannel } });
  const text = reply0(i).description;
  assert.match(text, /• \*\*Name:\*\* edit-me → renamed/);
  assert.match(text, /• \*\*Enabled:\*\* on → off/);
  assert.match(text, /Existing reposts stay in <#.*>; new reposts go to <#.*>\./);
  assert.match(text, /This board is \*\*off\*\*\. Turn it back on with `\/starboard edit name:renamed enabled:True`\./);
  assert.equal((await prisma.starboardEntry.findFirst({ where: { starboardId: board.id } })).boardMessageId, null, 'live entries are dropped');
  const after = await sb.getBoardByName(world.guildId, 'renamed');
  assert.equal(after.channelId, newBoardChannel.id);

  // Renaming onto an existing name.
  await sb.createBoard(world.guildId, { name: 'taken', channelId: world.boardChannel.id });
  i = await run({ strings: { name: 'renamed', rename: 'taken' } });
  assert.match(titleOf(i), /Name Taken/);

  // Missing permissions on the new channel; a deleted board.
  const noPerm = { ...newBoardChannel, id: sid('4', 318), permissionsFor: () => ({ has: () => false }) };
  i = await run({ strings: { name: 'renamed' }, channels: { channel: noPerm } });
  assert.match(titleOf(i), /Missing Permissions/);
  // allow_nsfw needs an age-restricted channel; with one it works.
  world.boardChannel.nsfw = true;
  const nsfwBoard = await sb.createBoard(world.guildId, { name: 'nsfw-board', channelId: world.boardChannel.id });
  i = await run({ strings: { name: 'nsfw-board' }, bools: { allow_nsfw: true } });
  assert.match(titleOf(i), /Starboard Updated/);
  assert.equal((await sb.getBoardByName(world.guildId, 'nsfw-board')).allowNsfw, true);
  world.boardChannel.nsfw = false;
  i = await run({ strings: { name: 'nsfw-board' }, ints: { threshold: 9 } });
  assert.match(titleOf(i), /Channel Not Age-Restricted/, 'effective values: the channel is no longer age-restricted');
  assert.ok(nsfwBoard);
});

test('DB: /starboard list, delete and autocomplete', { skip: skipDb }, async () => {
  await setupGuild(32);
  const world = makeWorld(32);
  const runList = async () => { const i = makeInteraction(world, { sub: 'list' }); await cmd.execute(i, world.client); return i; };

  let i = await runList();
  assert.match(titleOf(i), /Starboards/);
  assert.match(reply0(i).description, /No starboards yet/);

  const a = await mkBoard(32, { name: 'alpha', emojis: [], ignoredChannelIds: ['c-ig1', 'c-ig2'] });
  const gone = await sb.createBoard(world.guildId, { name: 'gone', channelId: sid('4', 999), enabled: false });
  await prisma.starboardEntry.create({ data: { starboardId: a.id, sourceChannelId: 'c', sourceMessageId: 's', authorId: 'a', boardMessageId: 'live', count: 3 } });
  i = await runList();
  const e = reply0(i);
  assert.equal(e.description, '2/10 boards');
  assert.equal(e.fields.length, 2);
  assert.equal(e.fields[0].name, '🌟 alpha');
  assert.match(e.fields[0].value, /Emojis: any emoji/);
  assert.match(e.fields[0].value, /Reposts: 1/);
  assert.match(e.fields[0].value, /Ignored: <#c-ig1>, <#c-ig2>/);
  assert.doesNotMatch(e.fields[0].value, /⚠️/);
  assert.equal(e.fields[1].name, '🌟 gone (off)');
  assert.match(e.fields[1].value, /⚠️ Channel deleted — set a new one with \/starboard edit/);
  world.guild.channels.cache.set(sid('4', 999), { ...world.boardChannel, id: sid('4', 999), permissionsFor: () => ({ has: (f) => f !== PermissionFlagsBits.SendMessages }) });
  i = await runList();
  assert.match(reply0(i).fields[1].value, /⚠️ I can't post there \(missing Send Messages\)/);

  // Ten verbose boards stay inside Discord's 6000-character embed total.
  await prisma.starboard.deleteMany({ where: { guildId: world.guildId } });
  await sb.invalidateBoards(world.guildId);
  const ids = Array.from({ length: 40 }, (_, n) => sid('3', n + 1));
  for (let n = 0; n < LIMITS.STARBOARDS; n += 1) {
    await sb.createBoard(world.guildId, {
      name: `board-with-a-long-name-${n}`.padEnd(32, 'x'), channelId: sid('4', 900 + n), ignoredChannelIds: ids,
      emojis: ['123456789012345678', '⭐', '🌟', '💫', '✨', '🔥', '💎', '🎉', '👍', '👏'],
    });
  }
  i = await runList();
  const big = reply0(i);
  const total = (big.title?.length ?? 0) + (big.description?.length ?? 0) + big.fields.reduce((n, f) => n + f.name.length + f.value.length, 0);
  assert.equal(big.fields.length, 10);
  assert.ok(total <= 6000, `list embed total ${total}`);
  assert.ok(big.fields.every((f) => f.value.length <= 1024));
  await prisma.starboard.deleteMany({ where: { guildId: world.guildId } });
  await sb.invalidateBoards(world.guildId);

  // delete: not found is ephemeral; autocomplete answers from the database.
  const del = makeInteraction(world, { sub: 'delete', strings: { name: 'nope' } });
  await cmd.execute(del, world.client);
  assert.equal(del.ephemeral, true);
  assert.match(titleOf(del), /Not Found/);

  await mkBoard(32, { name: 'zeta' });
  world.guild.channels.cache.set(world.boardChannel.id, { ...world.boardChannel, name: 'hall' });
  const ac = makeInteraction(world, { sub: 'edit', focused: 'ZE' });
  await cmd.autocomplete(ac);
  assert.deepEqual(ac.responded, [{ name: 'zeta → #hall', value: 'zeta' }]);
  const ac2 = makeInteraction(world, { sub: 'edit', focused: '' });
  await cmd.autocomplete(ac2);
  assert.equal(ac2.responded.length, 1);
});
