// Bump Reminders tests. Offline (no Discord, Postgres or Redis): the pure success check and bumper lookup, recording a bump
// (state guard, reminder job, clamped count), the botMessage hook end to end (module gate, thank-you, reward), the
// bump.remind job (claim, role ping rules, retries, module off), the cleanup hooks, the plugin declaration and the /bumps
// payload and subcommands, all against in-memory BumpState / BumpCount tables and fake channels.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const { PermissionFlagsBits, ChannelType } = await import('discord.js');
const svc = await import('../src/services/bumpService.js');
const bumpsPlugin = await import('../src/plugins/bumps.js');
const cmd = await import('../src/commands/community/bumps.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { clearHooks, registerHook, runHooks, createHookContext } = await import('../src/handlers/hookRegistry.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { JOB_TYPES } = await import('../src/config/jobTypes.js');
const { PERMISSION_LEVELS, DEFAULT_MODULE_SETTINGS } = await import('../src/config/constants.js');
const { MODULES } = await import('../src/config/modules.js');
const { DEFAULT_TEXTS } = await import('../src/dashboard/fields.js');
const { updateSettings, getSettings } = await import('../src/services/settingsService.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');
const { registerProvider } = await import('../src/services/rewards.js');

const {
  BUMP_SITES, BUMP_COUNT_MAX, DEFAULT_REMINDER_MESSAGE, DEFAULT_THANKS_MESSAGE, RAW_SQL, isDisboardSuccess, bumperOf,
  siteKeyForBot, isBumpSiteMessage, moduleOffEmbed, reminderDedupeKey, recordBump, onBotMessage, handleRemindJob,
  onChannelDelete, onRoleDelete, channelDeleteFilter, roleDeleteFilter, topBumpers, resetCounts,
} = svc;
const { plugin } = bumpsPlugin;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const states = new Map();      // `${guildId}|${site}` -> row
const counts = new Map();      // `${guildId}|${userId}` -> row
const settingsRows = new Map();
let countFails = false;
const rawLog = [];             // clamp argument of every count upsert

const copy = (row) => (row ? { ...row } : null);
const same = (a, b) => (a instanceof Date && b instanceof Date ? a.getTime() === b.getTime() : a === b);
const matches = (row, where = {}) => Object.entries(where).every(([k, v]) => {
  if (v instanceof Date || v === null || typeof v !== 'object') return same(row[k], v);
  if ('lt' in v) return row[k] < v.lt;
  if ('gt' in v) return row[k] > v.gt;
  return false;
});
const sortBy = (rows, orderBy) => {
  const keys = (Array.isArray(orderBy) ? orderBy : [orderBy]).filter(Boolean).flatMap((o) => Object.entries(o));
  return [...rows].sort((a, b) => {
    for (const [key, dir] of keys) {
      if (a[key] === b[key]) continue;
      return (a[key] > b[key] ? 1 : -1) * (dir === 'desc' ? -1 : 1);
    }
    return 0;
  });
};

const stateModel = {
  findUnique: async ({ where }) => copy(states.get(`${where.guildId_site.guildId}|${where.guildId_site.site}`)),
  findMany: async ({ where }) => [...states.values()].filter((r) => matches(r, where)).map(copy),
  updateMany: async ({ where, data }) => {
    const rows = [...states.values()].filter((r) => matches(r, where));
    for (const r of rows) Object.assign(r, data);
    return { count: rows.length };
  },
  create: async ({ data }) => {
    const key = `${data.guildId}|${data.site}`;
    if (states.has(key)) throw Object.assign(new Error('Unique constraint failed'), { code: 'P2002' });
    states.set(key, { remindedAt: null, ...data });
    return copy(states.get(key));
  },
};
const countModel = {
  findUnique: async ({ where }) => copy(counts.get(`${where.guildId_userId.guildId}|${where.guildId_userId.userId}`)),
  findMany: async ({ where, orderBy, take }) => sortBy([...counts.values()].filter((r) => matches(r, where)), orderBy).slice(0, take ?? 1e9).map(copy),
  deleteMany: async ({ where }) => {
    const rows = [...counts.entries()].filter(([, r]) => matches(r, where));
    for (const [k] of rows) counts.delete(k);
    return { count: rows.length };
  },
};
const queryRaw = async (q) => {
  if (String(q.sql).includes('"BumpCount"')) {
    if (countFails) throw new Error('db down');
    const [guildId, userId, , max] = q.values;
    rawLog.push(max);
    const key = `${guildId}|${userId}`;
    const row = counts.get(key) ?? { guildId, userId, count: 0, lastBumpAt: null };
    row.count = Math.min(row.count + 1, Number(max));
    counts.set(key, row);
    return [{ count: row.count }];
  }
  return [{ ok: 1 }];
};

const real = {
  bumpState: prisma.bumpState, bumpCount: prisma.bumpCount, guildSettings: prisma.guildSettings, $queryRaw: prisma.$queryRaw,
};
prisma.bumpState = stateModel;
prisma.bumpCount = countModel;
prisma.$queryRaw = queryRaw;
prisma.guildSettings = {
  findUnique: async ({ where }) => copy(settingsRows.get(where.guildId)),
  upsert: async ({ where, update }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId }), ...update };
    settingsRows.set(where.guildId, row);
    return copy(row);
  },
};

// The scheduler is stubbed through the service's seam.
const scheduled = [];
let scheduleFailures = 0;
svc.__setScheduleImpl(async (job) => {
  if (scheduleFailures > 0) { scheduleFailures -= 1; throw new Error('db down'); }
  scheduled.push(job);
  return { id: 'J', runAt: job.runAt };
});

// The coins provider of the economy package is replaced by a recorder.
const grants = [];
let grantResult = { ok: true, total: 10 };
registerProvider('coins', { grant: async (args) => { grants.push(args); return grantResult; }, take: async () => ({ ok: true }) }, 'bumps-test');

after(async () => {
  Object.assign(prisma, real);
  svc.__setScheduleImpl();
  clearHooks();
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

let nextId = 7000;
const DISBOARD = BUMP_SITES.disboard.botId;
const COOLDOWN = BUMP_SITES.disboard.cooldownMs;
const unix = (ms) => Math.floor(ms / 1000);

function fakeChannel(id, { guild, deny = [], failSend = null } = {}) {
  const denied = new Set(deny.map((k) => PermissionFlagsBits[k]));
  const ch = {
    id, guild, sent: [], type: ChannelType.GuildText,
    permissionsFor: () => ({ has: (bit) => !denied.has(bit) }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) {
      if (failSend) throw failSend;
      this.sent.push(payload);
      return { id: String(nextId++) };
    },
  };
  return ch;
}

function fakeGuild(id, { roles = [], ownerId = 'OWNER' } = {}) {
  const guild = {
    id, name: 'Test Guild', available: true, ownerId,
    channels: { cache: new Map(), fetch: async (cid) => { throw Object.assign(new Error('Unknown Channel'), { code: 10003, cid }); } },
    roles: { cache: new Map(roles.map((r) => [r.id, r])) },
    members: { cache: new Map(), me: { id: 'BOT' } },
  };
  guild.addChannel = (cid, opts = {}) => {
    const ch = fakeChannel(cid, { guild, ...opts });
    guild.channels.cache.set(cid, ch);
    return ch;
  };
  return guild;
}

function fakeBumpMessage(channel, {
  id = String(nextId++), bumperId = 'U1', description = 'Bump done! :thumbsup:', image = null, botId = DISBOARD, at = Date.now(),
} = {}) {
  return {
    id, author: { id: botId, bot: true }, webhookId: 'APP', channelId: channel.id, channel, createdTimestamp: at,
    embeds: [{ description, image: image ? { url: image } : null }],
    interactionMetadata: bumperId ? { user: { id: bumperId, bot: false, username: `user${bumperId}` } } : null,
  };
}

const ctxFor = (guild, message) => createHookContext('botMessage', { client: { user: { id: 'BOT' } }, guild, message, channel: message.channel });

async function configure(guildId, data = {}) {
  await updateSettings(guildId, {
    bumpsEnabled: true, bumpChannelId: null, bumpRoleId: null, bumpReminderMessage: null, bumpThanks: true, bumpThanksMessage: null,
    bumpRewardCoins: 0, economyEnabled: false, ...data,
  });
}

function reset() {
  states.clear();
  counts.clear();
  scheduled.length = 0;
  grants.length = 0;
  rawLog.length = 0;
  scheduleFailures = 0;
  countFails = false;
  grantResult = { ok: true, total: 10 };
  svc.__resetSeen();
}

/** Fake interaction: records every reply. `vals` feeds the option getters. */
function fakeInteraction({ guild, user = { id: 'ADMIN' }, sub, vals = {}, channel = null } = {}) {
  const i = {
    id: String(nextId++), createdTimestamp: Date.now(), guild, guildId: guild.id, user, member: { id: user.id },
    channel: channel ?? guild.channels.cache.values().next().value ?? null,
    deferred: false, replied: false, ephemeral: null, out: [], lastEdit: null,
    options: {
      getSubcommand: () => sub,
      getChannel: (n) => vals[n] ?? null,
      getRole: (n) => vals[n] ?? null,
      getUser: (n) => vals[n] ?? null,
      getBoolean: (n) => vals[n] ?? null,
      getInteger: (n) => vals[n] ?? null,
      getString: (n) => vals[n] ?? null,
    },
    async deferReply({ ephemeral } = {}) { this.deferred = true; this.ephemeral = Boolean(ephemeral); },
    async reply(p) { this.replied = true; this.out.push({ kind: 'reply', ...p }); return { resource: { message: null } }; },
    async editReply(p) { this.replied = true; this.lastEdit = p; this.out.push({ kind: 'edit', ...p }); return this.confirmMessage ?? {}; },
    async followUp(p) { this.out.push({ kind: 'followUp', ...p }); return {}; },
  };
  return i;
}
const embedOf = (i, n = -1) => {
  const payloads = i.out.filter((o) => o.embeds?.length);
  return payloads.at(n)?.embeds[0].toJSON();
};
const lastOut = (i) => i.out.at(-1);

/** A confirm message whose collector answers `yes` or `no` as soon as awaitConfirm listens. */
function answerConfirm(i, answer) {
  i.confirmMessage = {
    id: 'CONFIRM',
    createMessageComponentCollector() {
      const collector = new EventEmitter();
      setImmediate(() => {
        const ids = i.lastEdit.components[0].components.map((c) => c.data.custom_id);
        const picked = { customId: ids[answer === 'yes' ? 0 : 1], async update() {} };
        collector.emit('collect', picked);
        collector.emit('end');
      });
      return collector;
    },
  };
}

// ── Pure rules ────────────────────────────────────────────────────────────────

test('the site table holds Disboard with its bot id, name and two hour cooldown', () => {
  assert.deepEqual(Object.keys(BUMP_SITES), ['disboard']);
  assert.equal(BUMP_SITES.disboard.botId, '302050872383242240');
  assert.equal(BUMP_SITES.disboard.name, 'Disboard');
  assert.equal(COOLDOWN, 2 * 60 * 60 * 1000);
  assert.equal(typeof BUMP_SITES.disboard.isSuccess, 'function');
  assert.ok(Object.isFrozen(BUMP_SITES) && Object.isFrozen(BUMP_SITES.disboard));
  assert.equal(siteKeyForBot('302050872383242240'), 'disboard');
  assert.equal(siteKeyForBot('1'), null);
  assert.equal(siteKeyForBot(undefined), null);
});

test('isDisboardSuccess: the success fixtures', () => {
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'Bump done! :thumbsup:\nCheck it out [on DISBOARD](https://disboard.org/server/1)' }] }), true);
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'BUMP DONE!' }] }), true, 'case-insensitive');
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'Server bumped!', image: { url: 'https://disboard.org/images/bot-command-image-bump.png' } }] }), true, 'the image marks it even when the text is translated');
  assert.equal(isDisboardSuccess({ embeds: [{ data: { description: 'Bump done!' } }] }), true, 'raw embed data');
  assert.equal(isDisboardSuccess({ embeds: [{ data: { image: { url: 'https://x/bot-command-image-bump.png' } } }] }), true);
});

test('isDisboardSuccess: failures and other messages are not bumps', () => {
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'Please wait another 87 minutes until the server can be bumped' }] }), false);
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'Please wait another 3 minutes until the server can be bumped', image: { url: 'https://disboard.org/images/bot-command-image-bump.png' } }] }), false, 'the wait text wins over the image');
  assert.equal(isDisboardSuccess({ embeds: [{ description: 'Looking for a new server? Try DISBOARD.' }] }), false);
  assert.equal(isDisboardSuccess({ embeds: [{}] }), false);
  assert.equal(isDisboardSuccess({ embeds: [] }), false);
  assert.equal(isDisboardSuccess({}), false);
  assert.equal(isDisboardSuccess(null), false);
  assert.equal(isDisboardSuccess({ embeds: 'nope' }), false);
});

test('bumperOf: interaction metadata first, the deprecated interaction second, never a bot', () => {
  const a = { id: 'A' };
  const b = { id: 'B' };
  assert.equal(bumperOf({ interactionMetadata: { user: a }, interaction: { user: b } }), a);
  assert.equal(bumperOf({ interactionMetadata: null, interaction: { user: b } }), b);
  assert.equal(bumperOf({ interaction: { user: b } }), b);
  assert.equal(bumperOf({ interactionMetadata: { user: { id: 'X', bot: true } } }), null);
  assert.equal(bumperOf({}), null);
  assert.equal(bumperOf(null), null);
});

test('the hook filter is synchronous and passes only a known bump bot', () => {
  assert.equal(isBumpSiteMessage({ message: { author: { id: DISBOARD } } }), true);
  assert.equal(isBumpSiteMessage({ message: { author: { id: '999' } } }), false);
  assert.equal(isBumpSiteMessage({ message: {} }), false);
  assert.equal(isBumpSiteMessage({}), false);
  assert.notEqual(isBumpSiteMessage.constructor.name, 'AsyncFunction');
});

test('the default texts are exact copies of the dashboard defaults', () => {
  assert.equal(DEFAULT_REMINDER_MESSAGE, DEFAULT_TEXTS.bumpReminderMessage);
  assert.equal(DEFAULT_THANKS_MESSAGE, DEFAULT_TEXTS.bumpThanksMessage);
});

test('the module-off embed uses the router wording', () => {
  const e = moduleOffEmbed().toJSON();
  assert.match(e.title, /Module Disabled/);
  assert.equal(e.description, `The **${MODULES.bumps.label}** module is turned off on this server. An admin can enable it with \`/module enable bumps\`.`);
});

test('the reminder dedupe key matches the registered job type', () => {
  assert.equal(reminderDedupeKey('G1', 'disboard'), 'bump.remind:G1:disboard');
  assert.equal(JOB_TYPES['bump.remind'].dedupe, 'bump.remind:{guildId}:{site}');
  assert.equal(JOB_TYPES['bump.remind'].owner, 'bumps');
  assert.equal(JOB_TYPES['bump.remind'].recurring, false);
});

test('RAW_SQL: unique names, the count upsert clamps in bigint maths', () => {
  const names = RAW_SQL.map((r) => r.name);
  assert.equal(new Set(names).size, names.length);
  assert.ok(names.every((n) => n.startsWith('bumps.')));
  const q = RAW_SQL.find((r) => r.name === 'bumps.incrementCount').build();
  assert.match(q.sql, /ON CONFLICT \("guildId", "userId"\)/);
  assert.match(q.sql, /LEAST\("BumpCount"\."count"::bigint \+ 1, \?::bigint\)::int/);
  assert.ok(q.values.includes(BUMP_COUNT_MAX));
  assert.equal(BUMP_COUNT_MAX, 1_000_000_000);
});

// ── recordBump ────────────────────────────────────────────────────────────────

test('recordBump: the first bump writes the state, schedules the reminder and counts', async () => {
  reset();
  const at = new Date('2026-10-21T12:00:00Z');
  const out = await recordBump({ guildId: 'R1', site: 'disboard', channelId: 'C1', bumperId: 'U1', at });
  assert.equal(out.recorded, true);
  assert.equal(out.count, 1);
  assert.equal(out.nextBumpAt.getTime(), at.getTime() + COOLDOWN);
  const row = states.get('R1|disboard');
  assert.equal(row.lastBumperId, 'U1');
  assert.equal(row.channelId, 'C1');
  assert.equal(row.remindedAt, null);
  assert.equal(row.nextBumpAt.getTime(), at.getTime() + COOLDOWN);
  assert.equal(scheduled.length, 1);
  assert.deepEqual(scheduled[0], {
    guildId: 'R1', type: 'bump.remind', runAt: out.nextBumpAt, payload: { site: 'disboard' }, dedupeKey: 'bump.remind:R1:disboard', onConflict: 'replace',
  });
  assert.equal(counts.get('R1|U1').count, 1);
  assert.deepEqual(rawLog, [BUMP_COUNT_MAX]);
});

test('recordBump: the same bump again, or an older one, changes nothing', async () => {
  reset();
  const at = new Date('2026-10-21T12:00:00Z');
  await recordBump({ guildId: 'R2', site: 'disboard', channelId: 'C1', bumperId: 'U1', at });
  const again = await recordBump({ guildId: 'R2', site: 'disboard', channelId: 'C1', bumperId: 'U1', at });
  assert.deepEqual(again, { recorded: false, count: null, nextBumpAt: null });
  const older = await recordBump({ guildId: 'R2', site: 'disboard', channelId: 'C9', bumperId: 'U2', at: new Date(at.getTime() - 1000) });
  assert.equal(older.recorded, false);
  assert.equal(scheduled.length, 1);
  assert.equal(counts.size, 1);
  assert.equal(states.get('R2|disboard').channelId, 'C1');
});

test('recordBump: a newer bump replaces the state, clears remindedAt and raises the count', async () => {
  reset();
  const at = new Date('2026-10-21T12:00:00Z');
  await recordBump({ guildId: 'R3', site: 'disboard', channelId: 'C1', bumperId: 'U1', at });
  states.get('R3|disboard').remindedAt = new Date();
  const later = new Date(at.getTime() + COOLDOWN + 60_000);
  const out = await recordBump({ guildId: 'R3', site: 'disboard', channelId: 'C2', bumperId: 'U1', at: later });
  assert.equal(out.recorded, true);
  assert.equal(out.count, 2);
  const row = states.get('R3|disboard');
  assert.equal(row.channelId, 'C2');
  assert.equal(row.remindedAt, null);
  assert.equal(scheduled.length, 2);
  assert.equal(scheduled[1].runAt.getTime(), later.getTime() + COOLDOWN);
});

test('recordBump: a bumper unknown to the message still records the state and the reminder, but no count', async () => {
  reset();
  const out = await recordBump({ guildId: 'R4', site: 'disboard', channelId: 'C1', bumperId: null, at: new Date() });
  assert.equal(out.recorded, true);
  assert.equal(out.count, null);
  assert.equal(counts.size, 0);
  assert.equal(states.get('R4|disboard').lastBumperId, null);
  assert.equal(scheduled.length, 1);
});

test('recordBump: a failing count never costs the reminder', async () => {
  reset();
  countFails = true;
  const out = await recordBump({ guildId: 'R5', site: 'disboard', channelId: 'C1', bumperId: 'U1', at: new Date() });
  assert.equal(out.recorded, true);
  assert.equal(out.count, null);
  assert.equal(scheduled.length, 1);
});

test('recordBump: a failing schedule is retried once; a second failure is logged, not thrown', async () => {
  reset();
  scheduleFailures = 1;
  const a = await recordBump({ guildId: 'R6', site: 'disboard', channelId: 'C1', bumperId: 'U1', at: new Date() });
  assert.equal(a.recorded, true);
  assert.equal(scheduled.length, 1, 'the retry went through');
  reset();
  scheduleFailures = 2;
  const b = await recordBump({ guildId: 'R7', site: 'disboard', channelId: 'C1', bumperId: 'U1', at: new Date() });
  assert.equal(b.recorded, true);
  assert.equal(b.count, 1, 'the count still happens');
  assert.equal(scheduled.length, 0);
});

test('recordBump: an unknown site rejects', async () => {
  await assert.rejects(() => recordBump({ guildId: 'R8', site: 'nope', channelId: 'C1' }), /Unknown bump site/);
});

test('recordBump: two bumps racing for one guild run one at a time', async () => {
  reset();
  const at = new Date('2026-10-21T12:00:00Z');
  const [a, b] = await Promise.all([
    recordBump({ guildId: 'R9', site: 'disboard', channelId: 'C1', bumperId: 'U1', at }),
    recordBump({ guildId: 'R9', site: 'disboard', channelId: 'C1', bumperId: 'U2', at }),
  ]);
  assert.deepEqual([a.recorded, b.recorded].sort(), [false, true]);
  assert.equal(scheduled.length, 1);
  assert.equal(counts.size, 1, 'only the recorded bump is counted');
});

// ── The botMessage hook ───────────────────────────────────────────────────────

test('onBotMessage: a bump is recorded, the bumper thanked with the default text and only they can be pinged', async () => {
  reset();
  const guild = fakeGuild('B1');
  const channel = guild.addChannel('C1');
  await configure('B1');
  const at = Date.now() - 1000;
  const message = fakeBumpMessage(channel, { at });
  await onBotMessage(ctxFor(guild, message));
  assert.equal(states.get('B1|disboard').lastBumperId, 'U1');
  assert.equal(states.get('B1|disboard').channelId, 'C1');
  assert.equal(states.get('B1|disboard').lastBumpAt.getTime(), at);
  assert.equal(scheduled.length, 1);
  assert.equal(channel.sent.length, 1);
  const next = `<t:${unix(at + COOLDOWN)}:R>`;
  assert.equal(channel.sent[0].content, `📣 Thanks for bumping, <@U1>! The next bump is possible ${next}.`);
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], users: ['U1'] });
});

test('onBotMessage: a deferred answer counts once it is edited into "Bump done" (messageUpdate sends it as edited)', async () => {
  reset();
  const guild = fakeGuild('B9');
  const channel = guild.addChannel('C9');
  await configure('B9');
  const thinking = fakeBumpMessage(channel, { description: '' });
  thinking.embeds = [];
  await onBotMessage(ctxFor(guild, thinking));
  assert.equal(states.has('B9|disboard'), false, 'the placeholder is no bump');
  const done = { ...thinking, embeds: [{ description: 'Bump done! :thumbsup:', image: null }] };
  await onBotMessage(createHookContext('botMessage', { client: { user: { id: 'BOT' } }, guild, message: done, channel, edited: true }));
  assert.equal(states.get('B9|disboard').lastBumperId, 'U1');
  await onBotMessage(createHookContext('botMessage', { client: { user: { id: 'BOT' } }, guild, message: done, channel, edited: true }));
  assert.equal(channel.sent.length, 1, 'a second edit of the same answer is not a second bump');
});

test('onBotMessage: a bump in a thread is thanked with Send Messages in Threads (review2-B)', async () => {
  reset();
  const guild = fakeGuild('B10');
  await configure('B10');
  // The parent channel denies Send Messages; Send Messages in Threads is what posting in a thread needs.
  const thread = guild.addChannel('T1', { deny: ['SendMessages'] });
  thread.type = ChannelType.PublicThread;
  thread.isThread = () => true;
  await onBotMessage(ctxFor(guild, fakeBumpMessage(thread)));
  assert.equal(states.get('B10|disboard').channelId, 'T1');
  assert.equal(thread.sent.length, 1, 'thanked in the thread');
  // And the other way round: Send Messages alone is not enough in a thread, so no post is attempted.
  reset();
  const locked = guild.addChannel('T2', { deny: ['SendMessagesInThreads'] });
  locked.type = ChannelType.PublicThread;
  locked.isThread = () => true;
  await onBotMessage(ctxFor(guild, fakeBumpMessage(locked)));
  assert.equal(states.get('B10|disboard').channelId, 'T2', 'the bump is still recorded');
  assert.equal(locked.sent.length, 0);
});

test('onBotMessage: a custom thank-you gets {count}, {site} and {user}; its role and everyone mentions cannot ping', async () => {
  reset();
  const guild = fakeGuild('B2');
  const channel = guild.addChannel('C1');
  await configure('B2', { bumpThanksMessage: '@everyone <@&555> bump #{count} on {site} by {user}' });
  await recordBump({ guildId: 'B2', site: 'disboard', channelId: 'C1', bumperId: 'U1', at: new Date(Date.now() - 5 * COOLDOWN) });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel)));
  assert.equal(channel.sent[0].content, '@everyone <@&555> bump #2 on Disboard by <@U1>');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], users: ['U1'] });
});

test('onBotMessage: a failure reply, another bot and a repeated message are ignored', async () => {
  reset();
  const guild = fakeGuild('B3');
  const channel = guild.addChannel('C1');
  await configure('B3');
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel, { description: 'Please wait another 30 minutes until the server can be bumped' })));
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel, { botId: '123' })));
  assert.equal(states.size, 0);
  const message = fakeBumpMessage(channel);
  await onBotMessage(ctxFor(guild, message));
  await onBotMessage(ctxFor(guild, message));
  assert.equal(scheduled.length, 1);
  assert.equal(channel.sent.length, 1);
  assert.equal(counts.get('B3|U1').count, 1);
});

test('onBotMessage: thanks off, or no send permission, still records the bump', async () => {
  reset();
  const guild = fakeGuild('B4');
  const quietChannel = guild.addChannel('C1');
  await configure('B4', { bumpThanks: false });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(quietChannel)));
  assert.equal(quietChannel.sent.length, 0);
  assert.equal(states.get('B4|disboard').lastBumperId, 'U1');

  reset();
  const guild2 = fakeGuild('B5');
  const muted = guild2.addChannel('C1', { deny: ['SendMessages'] });
  await configure('B5');
  await onBotMessage(ctxFor(guild2, fakeBumpMessage(muted)));
  assert.equal(muted.sent.length, 0);
  assert.equal(scheduled.length, 1);
});

test('onBotMessage: a failing thank-you send does not stop the reward', async () => {
  reset();
  const guild = fakeGuild('B6');
  const channel = guild.addChannel('C1', { failSend: Object.assign(new Error('boom'), { code: 500 }) });
  await configure('B6', { bumpRewardCoins: 25 });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel)));
  assert.equal(states.has('B6|disboard'), true);
  assert.equal(grants.length, 1);
});

test('onBotMessage: no known bumper means no thank-you, no count and no reward, but the reminder is set', async () => {
  reset();
  const guild = fakeGuild('B7');
  const channel = guild.addChannel('C1');
  await configure('B7', { bumpRewardCoins: 10 });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel, { bumperId: null })));
  assert.equal(states.get('B7|disboard').lastBumperId, null);
  assert.equal(scheduled.length, 1);
  assert.equal(channel.sent.length, 0);
  assert.equal(counts.size, 0);
  assert.equal(grants.length, 0);
});

test('onBotMessage: bumpRewardCoins grants coins; module-disabled and other results are tolerated; 0 grants nothing', async () => {
  reset();
  const guild = fakeGuild('B8');
  const channel = guild.addChannel('C1');
  await configure('B8', { bumpRewardCoins: 25 });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel)));
  assert.equal(grants.length, 1);
  assert.equal(grants[0].userId, 'U1');
  assert.equal(grants[0].amount, 25);
  assert.equal(grants[0].guild, guild);
  assert.equal(grants[0].reason, 'Bumped the server');

  for (const result of [{ ok: false, reason: 'module-disabled' }, { ok: false, reason: 'error' }]) {
    grantResult = result;
    states.clear(); // a bump no newer than the recorded one is ignored
    await onBotMessage(ctxFor(guild, fakeBumpMessage(channel)));
  }
  assert.equal(grants.length, 3);

  reset();
  await configure('B8', { bumpRewardCoins: 0 });
  await onBotMessage(ctxFor(guild, fakeBumpMessage(channel)));
  assert.equal(grants.length, 0);
});

test('the plugin hook is gated by the module and skips other bots before any lookup', async () => {
  reset();
  clearHooks();
  for (const hook of plugin.hooks) registerHook(hook, { pluginId: plugin.id, pluginModule: plugin.module });
  const guild = fakeGuild('B9');
  const channel = guild.addChannel('C1');
  const run = (message) => runHooks('botMessage', { client: { user: { id: 'BOT' } }, guild, message, channel });

  await configure('B9', { bumpsEnabled: false });
  await run(fakeBumpMessage(channel));
  assert.equal(states.size, 0, 'module off: nothing recorded');

  await configure('B9');
  await run(fakeBumpMessage(channel, { botId: '42' }));
  assert.equal(states.size, 0, 'another bot: ignored');
  await run(fakeBumpMessage(channel));
  assert.equal(states.size, 1, 'module on, Disboard: recorded');
  clearHooks();
});

// ── The bump.remind job ───────────────────────────────────────────────────────

const roleOf = (id, mentionable) => ({ id, mentionable });
const dueState = (guildId, channelId = 'C1', extra = {}) => {
  const at = new Date(Date.now() - COOLDOWN - 60_000);
  states.set(`${guildId}|disboard`, {
    guildId, site: 'disboard', lastBumpAt: at, lastBumperId: 'U1', channelId, nextBumpAt: new Date(at.getTime() + COOLDOWN), remindedAt: null, ...extra,
  });
};
const job = (guildId, payload = { site: 'disboard' }) => ({ id: 'J1', guildId, type: 'bump.remind', payload });

test('bump.remind: posts the default reminder in the bump channel and marks it reminded', async () => {
  reset();
  const guild = fakeGuild('J1G');
  const channel = guild.addChannel('C1');
  await configure('J1G');
  dueState('J1G');
  const out = await handleRemindJob(job('J1G'), { guild });
  assert.equal(out, undefined);
  assert.equal(channel.sent.length, 1);
  assert.equal(channel.sent[0].content, DEFAULT_REMINDER_MESSAGE);
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], roles: [] });
  assert.ok(states.get('J1G|disboard').remindedAt instanceof Date);
});

test('bump.remind: running twice reminds once (the claim)', async () => {
  reset();
  const guild = fakeGuild('J2G');
  const channel = guild.addChannel('C1');
  await configure('J2G');
  dueState('J2G');
  await handleRemindJob(job('J2G'), { guild });
  await handleRemindJob(job('J2G'), { guild });
  assert.equal(channel.sent.length, 1);
});

test('bump.remind: two runs at once also remind once', async () => {
  reset();
  const guild = fakeGuild('J2H');
  const channel = guild.addChannel('C1');
  await configure('J2H');
  dueState('J2H');
  await Promise.all([handleRemindJob(job('J2H'), { guild }), handleRemindJob(job('J2H'), { guild })]);
  assert.equal(channel.sent.length, 1);
});

test('bump.remind: bumpChannelId wins over the channel of the last bump; a dead one falls back', async () => {
  reset();
  const guild = fakeGuild('J3G');
  const bump = guild.addChannel('C1');
  const chosen = guild.addChannel('C2');
  await configure('J3G', { bumpChannelId: 'C2' });
  dueState('J3G', 'C1');
  await handleRemindJob(job('J3G'), { guild });
  assert.equal(chosen.sent.length, 1);
  assert.equal(bump.sent.length, 0);

  reset();
  await configure('J3G', { bumpChannelId: 'GONE' });
  dueState('J3G', 'C1');
  await handleRemindJob(job('J3G'), { guild });
  assert.equal(bump.sent.length, 1, 'the channel of the last bump');
});

test('bump.remind: the role is pinged (only that role) when it is mentionable', async () => {
  reset();
  const guild = fakeGuild('J4G', { roles: [roleOf('R100', true)] });
  const channel = guild.addChannel('C1');
  await configure('J4G', { bumpRoleId: 'R100', bumpReminderMessage: 'Bump {site} now! @everyone <@&999>' });
  dueState('J4G');
  await handleRemindJob(job('J4G'), { guild });
  assert.equal(channel.sent[0].content, '<@&R100> Bump Disboard now! @everyone <@&999>');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], roles: ['R100'] });
});

test('bump.remind: an unmentionable role is not pinged unless the bot has Mention Everyone', async () => {
  reset();
  const guild = fakeGuild('J5G', { roles: [roleOf('R101', false)] });
  const channel = guild.addChannel('C1', { deny: ['MentionEveryone'] });
  await configure('J5G', { bumpRoleId: 'R101' });
  dueState('J5G');
  await handleRemindJob(job('J5G'), { guild });
  assert.equal(channel.sent[0].content, DEFAULT_REMINDER_MESSAGE, 'no mention in the text');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], roles: [] });

  reset();
  const guild2 = fakeGuild('J5H', { roles: [roleOf('R101', false)] });
  const strong = guild2.addChannel('C1');
  await configure('J5H', { bumpRoleId: 'R101' });
  dueState('J5H');
  await handleRemindJob(job('J5H'), { guild: guild2 });
  assert.deepEqual(strong.sent[0].allowedMentions, { parse: [], roles: ['R101'] });
});

test('bump.remind: a blank custom message falls back to the default', async () => {
  reset();
  const guild = fakeGuild('J6G');
  const channel = guild.addChannel('C1');
  await configure('J6G', { bumpReminderMessage: '   ' });
  dueState('J6G');
  await handleRemindJob(job('J6G'), { guild });
  assert.equal(channel.sent[0].content, DEFAULT_REMINDER_MESSAGE);
});

test('bump.remind: nothing happens while the module is off, without a state, or after the reminder', async () => {
  reset();
  const guild = fakeGuild('J7G');
  const channel = guild.addChannel('C1');
  await configure('J7G', { bumpsEnabled: false });
  dueState('J7G');
  assert.equal(await handleRemindJob(job('J7G'), { guild }), undefined);
  assert.equal(channel.sent.length, 0);
  assert.equal(states.get('J7G|disboard').remindedAt, null, 'the state is kept');

  await configure('J7G');
  states.clear();
  assert.equal(await handleRemindJob(job('J7G'), { guild }), undefined);
  dueState('J7G', 'C1', { remindedAt: new Date() });
  assert.equal(await handleRemindJob(job('J7G'), { guild }), undefined);
  assert.equal(channel.sent.length, 0);
});

test('bump.remind: a state whose next bump is still ahead reschedules the job for then', async () => {
  reset();
  const guild = fakeGuild('J8G');
  const channel = guild.addChannel('C1');
  await configure('J8G');
  const nextBumpAt = new Date(Date.now() + 30 * 60_000);
  dueState('J8G', 'C1', { nextBumpAt });
  const out = await handleRemindJob(job('J8G'), { guild });
  assert.equal(out.nextRunAt.getTime(), nextBumpAt.getTime());
  assert.equal(channel.sent.length, 0);
  assert.equal(states.get('J8G|disboard').remindedAt, null);
});

test('bump.remind: an unknown site is a permanent failure', async () => {
  const guild = fakeGuild('J9G');
  await assert.rejects(() => handleRemindJob(job('J9G', { site: 'nope' }), { guild }), PermanentJobError);
  await assert.rejects(() => handleRemindJob(job('J9G', {}), { guild }), PermanentJobError);
  await assert.rejects(() => handleRemindJob({ guildId: 'J9G' }, { guild }), PermanentJobError);
});

test('bump.remind: a transient send error releases the claim and is rethrown, so the retry reminds', async () => {
  reset();
  const guild = fakeGuild('J10G');
  const channel = guild.addChannel('C1');
  let fail = true;
  channel.send = async (payload) => {
    if (fail) { fail = false; throw Object.assign(new Error('Service Unavailable'), { status: 503 }); }
    channel.sent.push(payload);
    return { id: '1' };
  };
  await configure('J10G');
  dueState('J10G');
  await assert.rejects(() => handleRemindJob(job('J10G'), { guild }), /Service Unavailable/);
  assert.equal(states.get('J10G|disboard').remindedAt, null, 'released');
  await handleRemindJob(job('J10G'), { guild });
  assert.equal(channel.sent.length, 1);
  assert.ok(states.get('J10G|disboard').remindedAt);
});

test('bump.remind: a permanent send error (missing permissions) ends the job quietly and releases the claim', async () => {
  reset();
  const guild = fakeGuild('J11G');
  guild.addChannel('C1', { failSend: Object.assign(new Error('Missing Permissions'), { code: 50013 }) });
  await configure('J11G');
  dueState('J11G');
  assert.equal(await handleRemindJob(job('J11G'), { guild }), undefined);
  assert.equal(states.get('J11G|disboard').remindedAt, null);
});

test('bump.remind: no usable channel ends the job quietly and releases the claim', async () => {
  reset();
  const guild = fakeGuild('J12G');
  const channel = guild.addChannel('C1', { deny: ['SendMessages'] });
  await configure('J12G');
  dueState('J12G');
  assert.equal(await handleRemindJob(job('J12G'), { guild }), undefined);
  assert.equal(channel.sent.length, 0);
  assert.equal(states.get('J12G|disboard').remindedAt, null);

  reset();
  const bare = fakeGuild('J12H');
  await configure('J12H');
  dueState('J12H', 'NOPE');
  assert.equal(await handleRemindJob(job('J12H'), { guild: bare }), undefined);
});

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

test('channelDelete unsets only the reminder channel; the filter skips other channels', async () => {
  await configure('D1', { bumpChannelId: 'C1', bumpRoleId: 'R1' });
  assert.equal(channelDeleteFilter({ guildId: 'D1', channel: { id: 'C9' } }), false);
  assert.equal(channelDeleteFilter({ guildId: 'D1', channel: { id: 'C1' } }), true);
  assert.equal(channelDeleteFilter({ guildId: 'UNKNOWN', channel: { id: 'C1' } }), true, 'no memo: run');
  await onChannelDelete({ guildId: 'D1', channel: { id: 'C9' } });
  assert.equal((await getSettings('D1')).bumpChannelId, 'C1');
  await onChannelDelete({ guildId: 'D1', channel: { id: 'C1' } });
  const s = await getSettings('D1');
  assert.equal(s.bumpChannelId, null);
  assert.equal(s.bumpRoleId, 'R1');
});

test('roleDelete unsets only the ping role; the filter skips other roles', async () => {
  await configure('D2', { bumpChannelId: 'C1', bumpRoleId: 'R1' });
  assert.equal(roleDeleteFilter({ guildId: 'D2', role: { id: 'R9' } }), false);
  assert.equal(roleDeleteFilter({ guildId: 'D2', role: { id: 'R1' } }), true);
  await onRoleDelete({ guildId: 'D2', role: { id: 'R9' } });
  assert.equal((await getSettings('D2')).bumpRoleId, 'R1');
  await onRoleDelete({ guildId: 'D2', role: { id: 'R1' } });
  const s = await getSettings('D2');
  assert.equal(s.bumpRoleId, null);
  assert.equal(s.bumpChannelId, 'C1');
});

test('cleanup hooks never throw on missing ids or a settings failure', async () => {
  await onChannelDelete({});
  await onRoleDelete({});
  await onChannelDelete({ guildId: 'D3', channel: { id: 'C1' } });
});

// ── Reads ─────────────────────────────────────────────────────────────────────

test('topBumpers orders by count, then by the earlier last bump; resetCounts clears one or all', async () => {
  reset();
  const t = (n) => new Date(1_000_000 + n);
  counts.set('L|a', { guildId: 'L', userId: 'a', count: 3, lastBumpAt: t(5) });
  counts.set('L|b', { guildId: 'L', userId: 'b', count: 7, lastBumpAt: t(9) });
  counts.set('L|c', { guildId: 'L', userId: 'c', count: 3, lastBumpAt: t(1) });
  counts.set('L|z', { guildId: 'L', userId: 'z', count: 0, lastBumpAt: t(1) });
  counts.set('M|a', { guildId: 'M', userId: 'a', count: 99, lastBumpAt: t(1) });
  assert.deepEqual((await topBumpers('L')).map((r) => r.userId), ['b', 'c', 'a']);
  assert.deepEqual((await topBumpers('L', 2)).map((r) => r.userId), ['b', 'c']);
  assert.equal(await resetCounts('L', 'a'), 1);
  assert.equal(await resetCounts('L'), 3);
  assert.equal(counts.size, 1, 'another guild is untouched');
});

// ── Plugin declaration ────────────────────────────────────────────────────────

test('the plugin declares its hooks, job and module', async () => {
  assert.equal(plugin.id, 'bumps');
  assert.equal(plugin.module, 'bumps');
  assert.ok(MODULES.bumps);
  assert.equal(DEFAULT_MODULE_SETTINGS.bumps, false, 'off by default');
  const byEvent = Object.fromEntries(plugin.hooks.map((h) => [h.event, h]));
  assert.deepEqual(Object.keys(byEvent).sort(), ['botMessage', 'channelDelete', 'roleDelete']);
  for (const h of plugin.hooks) {
    assert.equal(h.id, 'bumps');
    assert.ok(HOOK_ORDER[h.event].includes('bumps'), `${h.event} lists bumps`);
    assert.equal(typeof h.run, 'function');
  }
  assert.equal(byEvent.botMessage.always, undefined, 'the botMessage hook is module-gated');
  assert.equal(typeof byEvent.botMessage.filter, 'function');
  assert.notEqual(byEvent.botMessage.filter.constructor.name, 'AsyncFunction');
  assert.equal(byEvent.channelDelete.always, true);
  assert.equal(byEvent.roleDelete.always, true);
  assert.deepEqual(plugin.components, []);
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['bump.remind']);
  assert.equal(typeof plugin.jobs[0].handler, 'function');
  assert.deepEqual(plugin.provides, {});
  const { errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /bumps/.test(e)), []);
});

// ── The /bumps command ────────────────────────────────────────────────────────

test('the /bumps payload: subcommands, permissions and size', () => {
  const payload = buildCommandPayload(cmd);
  assert.equal(payload.name, 'bumps');
  assert.deepEqual(payload.options.map((o) => o.name), ['status', 'leaderboard', 'setup', 'reset']);
  assert.equal(cmd.module, 'bumps');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.deepEqual({ ...cmd.subcommandPermissions }, { setup: PERMISSION_LEVELS.ADMIN, reset: PERMISSION_LEVELS.ADMIN });
  assert.equal(payload.default_member_permissions, undefined, 'members must see status and leaderboard');
  assert.ok(JSON.stringify(payload).length < 1500, `payload is ${JSON.stringify(payload).length} chars`);
  const setup = payload.options.find((o) => o.name === 'setup');
  assert.deepEqual(setup.options.map((o) => o.name), ['channel', 'role', 'thanks', 'coins']);
  assert.equal(setup.options.find((o) => o.name === 'coins').max_value, 100000);
  assert.equal(MODULES.bumps.configCommand, '/bumps');
});

test('/bumps status and leaderboard refuse while the module is off', async () => {
  reset();
  const guild = fakeGuild('K1');
  guild.addChannel('C1');
  await configure('K1', { bumpsEnabled: false });
  for (const sub of ['status', 'leaderboard']) {
    const i = fakeInteraction({ guild, user: { id: 'U1' }, sub });
    await cmd.execute(i);
    assert.equal(i.deferred, false);
    const out = lastOut(i);
    assert.equal(out.ephemeral, true);
    const e = out.embeds[0].toJSON();
    assert.match(e.title, /Module Disabled/);
    assert.match(e.description, /\/module enable bumps/);
  }
});

test('/bumps status: no bump yet, ready now, and waiting', async () => {
  reset();
  const guild = fakeGuild('K2');
  guild.addChannel('C1');
  await configure('K2');

  let i = fakeInteraction({ guild, user: { id: 'U1' }, sub: 'status' });
  await cmd.execute(i);
  assert.equal(i.ephemeral, false, 'public');
  assert.match(embedOf(i).fields[0].value, /No bump seen yet/);
  assert.deepEqual(lastOut(i).allowedMentions, { parse: [] });

  dueState('K2');
  i = fakeInteraction({ guild, user: { id: 'U1' }, sub: 'status' });
  await cmd.execute(i);
  let field = embedOf(i).fields[0];
  assert.equal(field.name, 'Disboard');
  assert.match(field.value, /Last bump: <t:\d+:R> by <@U1>/);
  assert.match(field.value, /Next bump: \*\*now\*\*/);

  const nextBumpAt = new Date(Date.now() + 40 * 60_000);
  states.get('K2|disboard').nextBumpAt = nextBumpAt;
  i = fakeInteraction({ guild, user: { id: 'U1' }, sub: 'status' });
  await cmd.execute(i);
  field = embedOf(i).fields[0];
  assert.ok(field.value.includes(`<t:${unix(nextBumpAt.getTime())}:R>`));
  assert.doesNotMatch(field.value, /\*\*now\*\*/);
});

test('/bumps leaderboard: empty, ranked with medals, and the caller outside the top ten', async () => {
  reset();
  const guild = fakeGuild('K3');
  guild.addChannel('C1');
  await configure('K3');

  let i = fakeInteraction({ guild, user: { id: 'me' }, sub: 'leaderboard' });
  await cmd.execute(i);
  assert.match(embedOf(i).description, /Nobody has bumped yet/);

  for (let n = 1; n <= 12; n += 1) counts.set(`K3|u${n}`, { guildId: 'K3', userId: `u${n}`, count: 100 - n, lastBumpAt: new Date(n) });
  counts.set('K3|me', { guildId: 'K3', userId: 'me', count: 1, lastBumpAt: new Date(1) });
  i = fakeInteraction({ guild, user: { id: 'me' }, sub: 'leaderboard' });
  await cmd.execute(i);
  const e = embedOf(i);
  const lines = e.description.split('\n');
  assert.equal(lines.length, 10);
  assert.equal(lines[0], '🥇 <@u1> — 99 bumps');
  assert.ok(lines[1].startsWith('🥈') && lines[2].startsWith('🥉'));
  assert.ok(lines[3].startsWith('**4.**'));
  assert.equal(e.footer.text, 'You: 1 bump');
  assert.deepEqual(lastOut(i).allowedMentions, { parse: [] });

  counts.set('K3|me', { guildId: 'K3', userId: 'me', count: 500, lastBumpAt: new Date(1) });
  i = fakeInteraction({ guild, user: { id: 'me' }, sub: 'leaderboard' });
  await cmd.execute(i);
  assert.equal(embedOf(i).footer, undefined, 'already on the board: no footer');
});

test('/bumps setup: no options shows the setup', async () => {
  reset();
  const guild = fakeGuild('K4');
  guild.addChannel('C1');
  await configure('K4', { bumpChannelId: 'C1', bumpRoleId: 'R5', bumpRewardCoins: 40 });
  const i = fakeInteraction({ guild, sub: 'setup' });
  await cmd.execute(i);
  assert.equal(i.ephemeral, true);
  const d = embedOf(i).description;
  assert.match(d, /Module: ✅ On/);
  assert.match(d, /<#C1>/);
  assert.match(d, /<@&R5>/);
  assert.match(d, /Coins per bump: 40/);
});

test('/bumps setup: saves the channel, role, thanks and coins', async () => {
  reset();
  const guild = fakeGuild('K5', { roles: [roleOf('R6', true)] });
  const channel = guild.addChannel('C1');
  await configure('K5', { bumpsEnabled: false });
  const i = fakeInteraction({ guild, sub: 'setup', vals: { channel: { id: 'C1' }, role: { id: 'R6' }, thanks: false, coins: 15 } });
  await cmd.execute(i);
  const s = await getSettings('K5');
  assert.equal(s.bumpChannelId, 'C1');
  assert.equal(s.bumpRoleId, 'R6');
  assert.equal(s.bumpThanks, false);
  assert.equal(s.bumpRewardCoins, 15);
  const e = embedOf(i);
  assert.match(e.title, /Bump Reminders Updated/);
  assert.match(e.description, /Module: ❌ Off/);
  const warnings = (e.fields ?? []).map((f) => f.value).join('\n');
  assert.match(warnings, /Economy module is off/);
  assert.doesNotMatch(warnings, /can't ping/);
  void channel;
});

test('/bumps setup: warns when the role cannot be pinged', async () => {
  reset();
  const guild = fakeGuild('K6', { roles: [roleOf('R7', false)] });
  guild.addChannel('C1', { deny: ['MentionEveryone'] });
  await configure('K6', { economyEnabled: true });
  const i = fakeInteraction({ guild, sub: 'setup', vals: { channel: { id: 'C1' }, role: { id: 'R7' } } });
  await cmd.execute(i);
  assert.equal((await getSettings('K6')).bumpRoleId, 'R7', 'still saved');
  const warning = embedOf(i).fields.map((f) => f.value).join('\n');
  assert.match(warning, /can't ping <@&R7>.*isn't mentionable/);
});

test('/bumps setup: refuses a channel the bot cannot post in and @everyone, saving nothing', async () => {
  reset();
  const guild = fakeGuild('K7');
  guild.addChannel('C1', { deny: ['SendMessages'] });
  await configure('K7', { bumpChannelId: null, bumpRoleId: null });
  let i = fakeInteraction({ guild, sub: 'setup', vals: { channel: { id: 'C1' }, coins: 50 } });
  await cmd.execute(i);
  assert.match(embedOf(i).title, /Cannot Use That Channel/);
  assert.match(embedOf(i).description, /Send Messages/);
  i = fakeInteraction({ guild, sub: 'setup', vals: { role: { id: 'K7' }, coins: 50 } });
  await cmd.execute(i);
  assert.match(embedOf(i).title, /Cannot Ping @everyone/);
  const s = await getSettings('K7');
  assert.equal(s.bumpChannelId, null);
  assert.equal(s.bumpRoleId, null);
  assert.equal(s.bumpRewardCoins, 0, 'nothing from a refused command is saved');
});

test('/bumps reset user clears just that member', async () => {
  reset();
  const guild = fakeGuild('K8');
  guild.addChannel('C1');
  counts.set('K8|a', { guildId: 'K8', userId: 'a', count: 4, lastBumpAt: new Date() });
  counts.set('K8|b', { guildId: 'K8', userId: 'b', count: 2, lastBumpAt: new Date() });
  let i = fakeInteraction({ guild, sub: 'reset', vals: { user: { id: 'a' } } });
  await cmd.execute(i);
  assert.equal(counts.has('K8|a'), false);
  assert.equal(counts.has('K8|b'), true);
  assert.match(embedOf(i).description, /bump count of <@a> was cleared/);
  i = fakeInteraction({ guild, sub: 'reset', vals: { user: { id: 'nobody' } } });
  await cmd.execute(i);
  assert.match(embedOf(i).description, /had no bumps counted/);
});

test('/bumps reset everyone asks first: Clear all empties the guild, Cancel changes nothing', async () => {
  reset();
  const guild = fakeGuild('K9');
  guild.addChannel('C1');
  counts.set('K9|a', { guildId: 'K9', userId: 'a', count: 4, lastBumpAt: new Date() });
  counts.set('K9|b', { guildId: 'K9', userId: 'b', count: 2, lastBumpAt: new Date() });
  counts.set('OTHER|a', { guildId: 'OTHER', userId: 'a', count: 9, lastBumpAt: new Date() });

  let i = fakeInteraction({ guild, sub: 'reset' });
  answerConfirm(i, 'no');
  await cmd.execute(i);
  assert.equal(counts.size, 3);

  i = fakeInteraction({ guild, sub: 'reset' });
  answerConfirm(i, 'yes');
  await cmd.execute(i);
  assert.equal(counts.size, 1, 'only the other guild is left');
  assert.match(embedOf(i).description, /Cleared the counts of 2 members/);
  assert.deepEqual(lastOut(i).allowedMentions, { parse: [] });
});
