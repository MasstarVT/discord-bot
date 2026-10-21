// Anti-Raid tests. Offline (no Discord, Postgres or Redis): the pure rules (join window, bit maths, account age), the
// lockdown start and end against an in-memory AntiRaidState table and fake guilds (saved bits before the role is edited,
// idempotent end, retry after a failed restore, one lockdown per burst), the join handler, the unlock job, the End
// button, the plugin declaration and the /antiraid payload and replies.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { PermissionFlagsBits, PermissionsBitField } from 'discord.js';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open and Postgres is unreachable.
process.env.REDIS_URL = 'redis://127.0.0.1:1';
process.env.DATABASE_URL = 'postgresql://test:test@127.0.0.1:1/test';

const svc = await import('../src/services/antiRaidService.js');
const raid = await import('../src/plugins/antiraid.js');
const cmd = await import('../src/commands/admin/antiraid.js');
const { default: redis } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { MOD_SOURCES } = await import('../src/services/moderationService.js');
const { PermanentJobError } = await import('../src/services/scheduler.js');

const {
  RAID, SEND_MASK, clampInt, harsher, splitSend, parsePrior, restoredBits, recordJoin, youngAccount, isActive, detectionOf,
  permissionProblems, configLines, buildStartEmbed, buildEndEmbed, buildYoungEmbed, startLockdown, endLockdown, resetMemory,
  moderationSource, peekLocked, botSendBits, isLocked, markActive,
} = svc;
const { handleMemberAdd, handleUnlockJob, handleEndButton, onChannelDelete, mayBeAlertChannel, plugin } = raid;

const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

// ── In-memory tables ──────────────────────────────────────────────────────────

const rows = new Map();          // AntiRaidState by guildId
const settingsRows = new Map();  // GuildSettings by guildId
const realState = prisma.antiRaidState;
const realSettings = prisma.guildSettings;
prisma.antiRaidState = {
  findUnique: async ({ where }) => (rows.has(where.guildId) ? { ...rows.get(where.guildId) } : null),
  upsert: async ({ where, create, update }) => {
    const row = rows.has(where.guildId) ? { ...rows.get(where.guildId), ...update } : { ...create };
    rows.set(where.guildId, row);
    return { ...row };
  },
  update: async ({ where, data }) => {
    const row = { ...rows.get(where.guildId), ...data };
    rows.set(where.guildId, row);
    return { ...row };
  },
};
prisma.guildSettings = {
  findUnique: async ({ where }) => (settingsRows.has(where.guildId) ? { ...settingsRows.get(where.guildId) } : null),
  upsert: async ({ where, update, create }) => {
    const row = { ...(settingsRows.get(where.guildId) ?? { guildId: where.guildId, ...create }), ...update };
    settingsRows.set(where.guildId, row);
    return { ...row };
  },
};

after(async () => {
  prisma.antiRaidState = realState;
  prisma.guildSettings = realSettings;
  Object.assign(console, quiet);
  redis.disconnect();
  await prisma.$disconnect().catch(() => {});
});

// ── Fakes ─────────────────────────────────────────────────────────────────────

const P = PermissionFlagsBits;
const VIEW = P.ViewChannel;
// @everyone: can view, send, send in threads, make public threads (not private ones), react.
const EVERYONE = VIEW | P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads | P.AddReactions | P.ReadMessageHistory;
const ALL_PERMS = ['ManageRoles', 'ManageGuild', 'KickMembers', 'BanMembers'];

let nextId = 1000;
function fakeChannel(id, guild, { allowed = true } = {}) {
  return {
    id, guildId: guild.id, sent: [], guild,
    permissionsFor: () => ({ has: () => allowed }),
    isTextBased: () => true,
    isThread: () => false,
    async send(payload) { this.sent.push(payload); return { id: String(nextId++) }; },
  };
}

// The bot's own role: holds the four send bits unless a test says otherwise (`botBits`).
const BOT_SEND = P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads | P.CreatePrivateThreads;

function fakeGuild(id, { perms = ALL_PERMS, everyone = EVERYONE, alertChannel = 'ALERT', botBits = BOT_SEND } = {}) {
  const guild = {
    id, ownerId: 'OWNER', client: { user: { id: 'BOT' } }, botBits: BigInt(botBits),
    incidentsData: null, incidentCalls: [], permissionCalls: [], failEveryone: null, failIncident: null,
    channels: { cache: new Map(), async fetch(cid) { return this.cache.get(cid) ?? null; } },
    async setIncidentActions(payload) {
      if (this.failIncident) throw this.failIncident;
      this.incidentCalls.push(payload);
      this.incidentsData = { invitesDisabledUntil: payload.invitesDisabledUntil, dmsDisabledUntil: payload.dmsDisabledUntil ?? null };
      return this.incidentsData;
    },
  };
  // Like discord.js, the bot's permissions are all its roles ORed together, @everyone's included.
  guild.members = {
    me: {
      get permissions() {
        return new PermissionsBitField(new PermissionsBitField(perms).bitfield | guild.botBits | guild.roles.everyone.permissions.bitfield);
      },
      roles: {
        get cache() {
          return new Map([
            [id, { id, permissions: { bitfield: guild.roles.everyone.permissions.bitfield } }],
            ['BOTROLE', { id: 'BOTROLE', permissions: { bitfield: guild.botBits } }],
          ]);
        },
      },
    },
  };
  guild.roles = {
    everyone: {
      permissions: { bitfield: BigInt(everyone) },
      async setPermissions(bits) {
        if (guild.failEveryone) throw guild.failEveryone;
        // Discord: without Administrator a bot may only add permissions that one of its own roles has.
        const added = BigInt(bits) & ~this.permissions.bitfield;
        if ((added & ~guild.botBits) !== 0n && !guild.members.me.permissions.has(P.Administrator)) {
          throw Object.assign(new Error('Missing Permissions'), { code: 50013 });
        }
        guild.permissionCalls.push({ bits: BigInt(bits), savedBefore: rows.get(id)?.priorEveryoneSend ?? null });
        this.permissions.bitfield = BigInt(bits);
      },
    },
  };
  if (alertChannel) guild.channels.cache.set(alertChannel, fakeChannel(alertChannel, guild));
  return guild;
}

const everyoneBits = (guild) => guild.roles.everyone.permissions.bitfield;

const S = (over = {}) => ({
  guildId: 'G', antiRaidEnabled: true, antiRaidJoinThreshold: 3, antiRaidJoinSeconds: 10, antiRaidAction: 'KICK',
  antiRaidPauseInvites: true, antiRaidLockChat: true, antiRaidLockdownMinutes: 30, antiRaidMinAccountAgeDays: 0,
  antiRaidYoungAction: 'KICK', antiRaidAlertChannelId: 'ALERT', modLogChannelId: null, ...over,
});

function fakeDeps() {
  const d = {
    jobs: [], cancelled: [], mod: [], failSchedule: false, modResult: { ok: true },
    ensureGuildSettings: async () => {},
    scheduleJob: async (job) => { if (d.failSchedule) throw new Error('db down'); d.jobs.push(job); return { id: 'J', runAt: job.runAt }; },
    cancelJobs: async (filter) => { d.cancelled.push(filter); return 1; },
    executeModAction: async (input) => { d.mod.push(input); return d.modResult; },
  };
  return d;
}

const OLD = Date.now() - 400 * 86_400_000;
let userSeq = 1;
function fakeMember(guild, { bot = false, created = OLD } = {}) {
  const id = `1${String(userSeq++).padStart(17, '0')}`;
  return { id, guild, joinedTimestamp: Date.now(), user: { id, bot, tag: `user${userSeq}#0`, createdTimestamp: created } };
}
const ctxFor = (guild, member, settings) => ({
  guild, guildId: guild.id, member, user: member.user, state: {}, getSettings: async () => settings,
});
const alerts = (guild, id = 'ALERT') => guild.channels.cache.get(id).sent;
const embedOf = (payload) => payload.embeds[0].toJSON();
const fieldOf = (payload, name) => embedOf(payload).fields?.find((f) => f.name === name)?.value;

// ── Pure rules ────────────────────────────────────────────────────────────────

test('clampInt, harsher and the moderation source', () => {
  assert.equal(clampInt(500, RAID.THRESHOLD, 10), 100);
  assert.equal(clampInt(1, RAID.THRESHOLD, 10), 3);
  assert.equal(clampInt('x', RAID.THRESHOLD, 10), 10);
  assert.equal(clampInt(12.9, RAID.THRESHOLD, 10), 12);
  assert.equal(harsher('ALERT', 'KICK'), 'KICK');
  assert.equal(harsher('BAN', 'KICK'), 'BAN');
  assert.equal(harsher('KICK', 'BAN'), 'BAN');
  assert.equal(harsher(null, 'ALERT'), 'ALERT');
  assert.equal(harsher('KICK', null), 'KICK');
  assert.equal(harsher(null, undefined), null);
  assert.equal(moderationSource(), MOD_SOURCES.includes('antiraid') ? 'antiraid' : 'automod');
  assert.ok(MOD_SOURCES.includes(moderationSource()), 'executeModAction accepts the source');
});

test('the chat lock only ever takes the four send bits and never View Channels', () => {
  assert.equal(SEND_MASK, P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads | P.CreatePrivateThreads);
  assert.equal(SEND_MASK & VIEW, 0n);
  const { taken, rest } = splitSend(EVERYONE);
  assert.equal(taken, P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads, 'exactly the bits it had');
  assert.equal(rest, VIEW | P.AddReactions | P.ReadMessageHistory);
  assert.equal(rest & VIEW, VIEW, 'View Channels stays');
  assert.equal(taken | rest, EVERYONE);
});

test('restoring ORs the saved bits into the current permissions and nothing else', () => {
  const taken = P.SendMessages | P.CreatePublicThreads;
  const current = VIEW | P.AddReactions | P.EmbedLinks; // an admin added Embed Links meanwhile
  assert.equal(restoredBits(current, taken.toString()), current | taken);
  assert.equal(restoredBits(current, null), current, 'no saved bits: unchanged');
  assert.equal(restoredBits(current, (taken | P.Administrator).toString()), current | taken, 'only send bits can come back');
  assert.equal(restoredBits(current, 'garbage'), current, 'a broken column restores nothing');
  assert.equal(parsePrior(null), null);
  assert.equal(parsePrior('0'), 0n);
});

test('the join window trips at the threshold, forgets old joins and clears after a trip', () => {
  resetMemory();
  const t0 = 1_000_000;
  assert.deepEqual(recordJoin('W1', t0, t0, 3, 10), { count: 1, tripped: false });
  assert.deepEqual(recordJoin('W1', t0 + 4000, t0 + 4000, 3, 10), { count: 2, tripped: false });
  assert.deepEqual(recordJoin('W1', t0 + 9000, t0 + 9000, 3, 10), { count: 3, tripped: true });
  assert.deepEqual(recordJoin('W1', t0 + 9500, t0 + 9500, 3, 10), { count: 1, tripped: false }, 'the burst is spent');

  // Two joins, a pause longer than the window, then one more: no raid.
  recordJoin('W2', t0, t0, 3, 10);
  recordJoin('W2', t0 + 1000, t0 + 1000, 3, 10);
  assert.equal(recordJoin('W2', t0 + 20_000, t0 + 20_000, 3, 10).tripped, false);
  // A join that happened before the window does not count.
  assert.equal(recordJoin('W3', t0 - 60_000, t0, 3, 10).count, 0);
  // Guilds do not share a window.
  recordJoin('W4', t0, t0, 3, 10);
  recordJoin('W4', t0, t0, 3, 10);
  assert.equal(recordJoin('W5', t0, t0, 3, 10).count, 1);
});

test('detection settings are clamped and account age is checked in whole days', () => {
  assert.deepEqual(detectionOf({ antiRaidJoinThreshold: 1, antiRaidJoinSeconds: 9999, antiRaidLockdownMinutes: 1 }),
    { threshold: 3, seconds: 300, minutes: 5 });
  assert.deepEqual(detectionOf(null), { threshold: 10, seconds: 10, minutes: 30 });

  const now = Date.now();
  const day = 86_400_000;
  assert.deepEqual(youngAccount({ createdTimestamp: now - 2.5 * day }, { antiRaidMinAccountAgeDays: 7 }, now), { ageDays: 2, minDays: 7 });
  assert.equal(youngAccount({ createdTimestamp: now - 8 * day }, { antiRaidMinAccountAgeDays: 7 }, now), null);
  assert.equal(youngAccount({ createdTimestamp: now - day }, { antiRaidMinAccountAgeDays: 0 }, now), null, '0 = off');
  assert.equal(youngAccount({}, { antiRaidMinAccountAgeDays: 7 }, now), null, 'no creation time: not young');
});

test('isActive needs a start and a future end', () => {
  const now = Date.now();
  assert.equal(isActive(null, now), false);
  assert.equal(isActive({ lockedAt: null, lockedUntil: null }, now), false);
  assert.equal(isActive({ lockedAt: new Date(now - 1000), lockedUntil: new Date(now - 1) }, now), false);
  assert.equal(isActive({ lockedAt: new Date(now - 1000), lockedUntil: new Date(now + 1000) }, now), true);
});

test('permissionProblems names what the settings need and the bot lacks', () => {
  const none = fakeGuild('PP1', { perms: [] });
  const all = fakeGuild('PP2');
  const settings = S({ antiRaidAction: 'BAN', antiRaidMinAccountAgeDays: 7, antiRaidYoungAction: 'KICK' });
  assert.deepEqual(permissionProblems(all, settings), []);
  const problems = permissionProblems(none, settings).join('\n');
  assert.match(problems, /Pausing invites: I need \*\*Manage Server\*\*/);
  assert.match(problems, /Locking chat: I need \*\*Manage Roles\*\*/);
  assert.match(problems, /Kicking members: I need \*\*Kick Members\*\*/);
  assert.match(problems, /Banning members: I need \*\*Ban Members\*\*/);
  // Nothing asked, nothing needed.
  assert.deepEqual(permissionProblems(none, S({
    antiRaidPauseInvites: false, antiRaidLockChat: false, antiRaidAction: 'ALERT', antiRaidMinAccountAgeDays: 0,
  })), []);
  assert.equal(configLines(settings).length, 5);
});

// ── Embeds ────────────────────────────────────────────────────────────────────

test('the start alert says what happened and the End button carries the registered prefix', async () => {
  const guild = fakeGuild('E1');
  const until = new Date(Date.now() + 30 * 60_000);
  const state = { lockedAt: new Date(), lockedUntil: until, lockedById: null, reason: null };
  const effects = { chat: 'locked', invites: 'paused', errors: [] };
  const embed = buildStartEmbed({ state, settings: S(), effects, trigger: { joins: 12, seconds: 10 }, problems: ['Kicking members: I need **Kick Members**.'] }).toJSON();
  assert.match(embed.title, /Raid detected/);
  assert.match(embed.description, /\*\*12\*\* members joined within \*\*10\*\* seconds/);
  const names = embed.fields.map((f) => f.name);
  assert.deepEqual(names, ['Ends', 'New members', 'Invites', 'Chat', 'Needs attention']);
  assert.match(embed.fields.find((f) => f.name === 'Needs attention').value, /Kick Members/);

  const manual = buildStartEmbed({ state: { ...state, lockedById: '123456789012345678', reason: 'spam wave' }, settings: S(), effects }).toJSON();
  assert.match(manual.description, /<@123456789012345678>: spam wave/);

  const posted = await svc.announceStart(guild, S(), { state, effects }, { joins: 12, seconds: 10 });
  assert.equal(posted.channel.id, 'ALERT');
  const row = alerts(guild).at(-1).components[0].toJSON();
  assert.equal(row.components[0].custom_id, 'antiraid:end');
  assert.deepEqual(alerts(guild).at(-1).allowedMentions, { parse: [] });
});

test('the alert falls back to the moderation log channel and posts nothing without a usable channel', async () => {
  const guild = fakeGuild('E2', { alertChannel: null });
  guild.channels.cache.set('MODLOG', fakeChannel('MODLOG', guild));
  const state = { lockedAt: new Date(), lockedUntil: new Date(Date.now() + 60_000) };
  const started = { state, effects: { chat: 'skipped', invites: 'skipped', errors: [] } };
  // The alert channel is deleted or unusable, the log channel works.
  const posted = await svc.announceStart(guild, S({ antiRaidAlertChannelId: 'GONE', modLogChannelId: 'MODLOG' }), started);
  assert.equal(posted.channel.id, 'MODLOG');
  assert.equal(await svc.announceStart(guild, S({ antiRaidAlertChannelId: null, modLogChannelId: null }), started), null);
});

test('the end note counts what the lockdown did and warns about a failed restore', () => {
  const ok = buildEndEmbed({ how: 'expired', result: { tally: { kicked: 4, banned: 2, failed: 1, flagged: 0 }, effects: { chat: 'restored', invites: 'resumed', errors: [] } } }).toJSON();
  assert.match(ok.description, /ran out/);
  assert.match(ok.description, /4 kicked, 2 banned, 1 could not be removed/);
  const bad = buildEndEmbed({ how: 'manual', endedById: '123456789012345678', result: { pendingRestore: true, effects: { chat: 'failed', invites: 'none', errors: ['Could not give @everyone back Send Messages.'] } } }).toJSON();
  assert.match(bad.description, /<@123456789012345678>/);
  assert.match(bad.fields.find((f) => f.name === 'Needs attention').value, /Send Messages/);
  assert.notEqual(ok.color, bad.color);
  assert.match(buildYoungEmbed({ member: { user: { id: '5', tag: 'x#0' } }, ageDays: 1, minDays: 7, action: 'KICK', more: 2 }).toJSON().fields.map((f) => f.value).join('|'), /1 day \(minimum 7\)\|Kicked\|2 more new accounts/);
});

// ── Starting and ending a lockdown ────────────────────────────────────────────

test('start saves the bits before editing @everyone, strips only the send bits and pauses invites', async () => {
  resetMemory();
  const guild = fakeGuild('L1');
  const d = fakeDeps();
  const before = Date.now();
  const res = await startLockdown({ guild, settings: S(), startedById: '123456789012345678', reason: 'spam wave' }, d);
  assert.equal(res.ok, true);
  assert.equal(res.started, true);
  assert.deepEqual(res.effects, { chat: 'locked', invites: 'paused', errors: [] });

  const row = rows.get('L1');
  const taken = P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads;
  assert.equal(row.priorEveryoneSend, taken.toString());
  assert.equal(row.lockedById, '123456789012345678');
  assert.equal(row.reason, 'spam wave');
  assert.ok(row.lockedUntil.getTime() - row.lockedAt.getTime() === 30 * 60_000);
  assert.ok(row.lockedAt.getTime() >= before);

  assert.equal(guild.permissionCalls.length, 1);
  assert.equal(guild.permissionCalls[0].savedBefore, taken.toString(), 'the bits were saved before the role was edited');
  assert.equal(everyoneBits(guild), VIEW | P.AddReactions | P.ReadMessageHistory, 'View Channels and the rest stay');

  assert.equal(guild.incidentCalls.length, 1);
  assert.equal(guild.incidentCalls[0].invitesDisabledUntil.getTime(), row.lockedUntil.getTime());
  assert.deepEqual(d.jobs.map((j) => [j.type, j.dedupeKey, j.runAt.getTime()]),
    [['antiraid.unlock', 'antiraid.unlock:L1', row.lockedUntil.getTime()]]);
  assert.equal(peekLocked('L1'), true);
});

test('the invite pause never passes Discord\'s 24 hour cap and keeps a longer pause', async () => {
  resetMemory();
  const guild = fakeGuild('L2');
  const res = await startLockdown({ guild, settings: S({ antiRaidLockChat: false }), minutes: 1440 }, fakeDeps());
  assert.equal(res.minutes, 1440);
  assert.ok(guild.incidentCalls[0].invitesDisabledUntil.getTime() - Date.now() < 24 * 3_600_000, 'under 24 h from now');

  const g2 = fakeGuild('L2b');
  g2.incidentsData = { invitesDisabledUntil: new Date(Date.now() + 40 * 60_000), dmsDisabledUntil: null };
  const r2 = await startLockdown({ guild: g2, settings: S({ antiRaidLockChat: false }) }, fakeDeps());
  assert.equal(r2.effects.invites, 'already');
  assert.equal(g2.incidentCalls.length, 0, 'a longer pause is left alone');
});

test('a burst of start calls makes one lockdown and never re-snapshots the stripped bits', async () => {
  resetMemory();
  const guild = fakeGuild('L3');
  const d = fakeDeps();
  const all = await Promise.all([1, 2, 3, 4, 5].map(() => startLockdown({ guild, settings: S() }, d)));
  assert.deepEqual(all.map((r) => r.started), [true, false, false, false, false]);
  assert.equal(guild.permissionCalls.length, 1, '@everyone is edited once');
  assert.equal(d.jobs.length, 1);
  const taken = P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads;
  assert.equal(rows.get('L3').priorEveryoneSend, taken.toString(), 'the snapshot is still what @everyone had');
  // And a call that comes later, while it is on, changes nothing either.
  const again = await startLockdown({ guild, settings: S() }, d);
  assert.equal(again.started, false);
  assert.equal(rows.get('L3').priorEveryoneSend, taken.toString());
});

test('missing permissions cost an effect, not the lockdown, and are reported', async () => {
  resetMemory();
  const guild = fakeGuild('L4', { perms: [] });
  const res = await startLockdown({ guild, settings: S() }, fakeDeps());
  assert.equal(res.started, true);
  assert.deepEqual([res.effects.chat, res.effects.invites], ['failed', 'failed']);
  assert.match(res.effects.errors.join('\n'), /Manage Roles/);
  assert.match(res.effects.errors.join('\n'), /Manage Server/);
  assert.equal(rows.get('L4').priorEveryoneSend ?? null, null, 'nothing was taken, nothing to give back');
  assert.equal(guild.permissionCalls.length, 0);

  // Discord refusing the edit resets the saved bits (a later end must not "restore" what was never removed).
  const g2 = fakeGuild('L4b');
  g2.failEveryone = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  const r2 = await startLockdown({ guild: g2, settings: S() }, fakeDeps());
  assert.equal(r2.effects.chat, 'failed');
  assert.equal(rows.get('L4b').priorEveryoneSend ?? null, null);
  assert.equal(everyoneBits(g2), EVERYONE);

  // Chat and invites off in the settings: nothing is touched.
  const g3 = fakeGuild('L4c');
  const r3 = await startLockdown({ guild: g3, settings: S({ antiRaidLockChat: false, antiRaidPauseInvites: false }) }, fakeDeps());
  assert.deepEqual([r3.effects.chat, r3.effects.invites], ['skipped', 'skipped']);
  assert.equal(g3.permissionCalls.length + g3.incidentCalls.length, 0);
});

test('an @everyone that cannot send already has nothing taken or restored', async () => {
  resetMemory();
  const guild = fakeGuild('L5', { everyone: VIEW });
  const d = fakeDeps();
  const res = await startLockdown({ guild, settings: S({ antiRaidPauseInvites: false }) }, d);
  assert.equal(res.effects.chat, 'nothing');
  assert.equal(guild.permissionCalls.length, 0);
  assert.equal(rows.get('L5').priorEveryoneSend ?? null, null);
  const end = await endLockdown({ guild, settings: S() }, d);
  assert.equal(end.wasActive, true);
  assert.equal(guild.permissionCalls.length, 0);
});

test('a job that cannot be scheduled undoes the lockdown', async () => {
  resetMemory();
  const guild = fakeGuild('L6');
  const d = fakeDeps();
  d.failSchedule = true;
  const res = await startLockdown({ guild, settings: S() }, d);
  assert.equal(res.ok, false);
  assert.equal(rows.get('L6').lockedAt, null);
  assert.equal(guild.permissionCalls.length, 0, 'nothing was changed on Discord');
  assert.equal(peekLocked('L6'), undefined, 'the memo does not claim a lockdown');
});

test('end restores exactly the saved bits, resumes invites, clears the job and is idempotent', async () => {
  resetMemory();
  const guild = fakeGuild('L7');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S() }, d);
  // An admin added Embed Links to @everyone while the lockdown was on: that must survive.
  guild.roles.everyone.permissions.bitfield |= P.EmbedLinks;

  const res = await endLockdown({ guild, settings: S() }, d);
  assert.equal(res.wasActive, true);
  assert.equal(res.restored, true);
  assert.deepEqual([res.effects.chat, res.effects.invites], ['restored', 'resumed']);
  assert.equal(everyoneBits(guild), EVERYONE | P.EmbedLinks);
  assert.equal(guild.incidentCalls.at(-1).invitesDisabledUntil, null);
  assert.deepEqual(d.cancelled, [{ dedupeKey: 'antiraid.unlock:L7' }]);
  const row = rows.get('L7');
  assert.deepEqual([row.lockedAt, row.lockedUntil, row.lockedById, row.reason, row.priorEveryoneSend], [null, null, null, null, null]);
  assert.equal(peekLocked('L7'), false);

  const calls = guild.permissionCalls.length;
  const again = await endLockdown({ guild, settings: S() }, d);
  assert.equal(again.wasActive, false, 'a second end finds nothing');
  assert.equal(guild.permissionCalls.length, calls, 'and edits nothing');
  assert.equal(guild.incidentCalls.length, 2);
  // Two ends at once: one does the work.
  await startLockdown({ guild, settings: S() }, d);
  const both = await Promise.all([endLockdown({ guild, settings: S() }, d), endLockdown({ guild, settings: S() }, d)]);
  assert.deepEqual(both.map((r) => r.wasActive).sort(), [false, true]);
});

test('end leaves an invite pause somebody else set', async () => {
  resetMemory();
  const guild = fakeGuild('L8');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S({ antiRaidLockChat: false }) }, d);
  guild.incidentsData = { invitesDisabledUntil: new Date(Date.now() + 5 * 3_600_000), dmsDisabledUntil: null }; // an admin's own pause
  const res = await endLockdown({ guild, settings: S() }, d);
  assert.equal(res.effects.invites, 'foreign');
  assert.equal(guild.incidentCalls.length, 1, 'only the pause at start');
});

test('a failed restore keeps the saved bits and the job, and ending again retries it', async () => {
  resetMemory();
  const guild = fakeGuild('L9');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S() }, d);
  const taken = (P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads).toString();
  guild.failEveryone = Object.assign(new Error('boom'), { code: 50013 });

  const res = await endLockdown({ guild, settings: S() }, d);
  assert.equal(res.wasActive, true);
  assert.equal(res.pendingRestore, true);
  assert.equal(res.permanent, true);
  assert.equal(res.effects.chat, 'failed');
  assert.match(res.effects.errors[0], /Send Messages/);
  assert.equal(rows.get('L9').lockedAt, null, 'the lockdown itself is over');
  assert.equal(rows.get('L9').priorEveryoneSend, taken, 'the bits are kept');
  assert.deepEqual(d.cancelled, [], 'the job stays so the scheduler can retry');
  assert.equal(peekLocked('L9'), false);

  guild.failEveryone = null;
  const retry = await endLockdown({ guild, settings: S() }, d);
  assert.equal(retry.wasActive, false, 'no lockdown any more');
  assert.equal(retry.restored, true, 'but the permissions come back');
  assert.equal(everyoneBits(guild), EVERYONE);
  assert.equal(rows.get('L9').priorEveryoneSend, null);
  assert.deepEqual(d.cancelled, [{ dedupeKey: 'antiraid.unlock:L9' }]);
});

test('a new lockdown keeps a restore that is still pending', async () => {
  resetMemory();
  const guild = fakeGuild('L10');
  const d = fakeDeps();
  const taken = P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads | P.CreatePrivateThreads;
  // An earlier lockdown ended but could not give back all four bits (the column still holds them).
  rows.set('L10', { guildId: 'L10', lockedAt: null, lockedUntil: null, lockedById: null, reason: null, priorEveryoneSend: taken.toString() });
  guild.roles.everyone.permissions.bitfield = VIEW;
  await startLockdown({ guild, settings: S({ antiRaidPauseInvites: false }) }, d);
  assert.equal(rows.get('L10').priorEveryoneSend, taken.toString(), 'the first lockdown\'s bits are not forgotten');
  await endLockdown({ guild, settings: S() }, d);
  assert.equal(everyoneBits(guild), VIEW | taken);
});

// ── Review fixes: bits the bot cannot give back, the memo race, the capped invite pause ─────────────────────

const SEND_BITS_NO_PRIVATE = P.SendMessages | P.SendMessagesInThreads | P.CreatePublicThreads;
const everyoneWithPrivate = EVERYONE | P.CreatePrivateThreads;

test('botSendBits counts the bot\'s own roles, not @everyone, and everything with Administrator', () => {
  assert.equal(botSendBits(fakeGuild('B1', { botBits: BOT_SEND })), SEND_MASK);
  // @everyone has Create Private Threads, so the bot's effective permissions do too; its own roles do not.
  const g = fakeGuild('B2', { everyone: everyoneWithPrivate, botBits: SEND_BITS_NO_PRIVATE | P.ManageRoles });
  assert.ok(g.members.me.permissions.has(P.CreatePrivateThreads), 'the effective permissions include @everyone');
  assert.equal(botSendBits(g), SEND_BITS_NO_PRIVATE, 'but only the roles of the bot count (and only send bits)');
  assert.equal(botSendBits(fakeGuild('B3', { perms: ['Administrator'], botBits: 0n })), SEND_MASK);
  assert.equal(botSendBits({ id: 'X', members: { me: null } }), 0n);
});

test('a chat lock leaves the send bits the bot cannot give back, names them and still ends cleanly', async () => {
  resetMemory();
  const guild = fakeGuild('H1', { everyone: everyoneWithPrivate, botBits: SEND_BITS_NO_PRIVATE });
  const d = fakeDeps();
  const res = await startLockdown({ guild, settings: S({ antiRaidPauseInvites: false }) }, d);
  assert.equal(res.started, true);
  assert.equal(res.effects.chat, 'partial');
  assert.match(res.effects.errors.join('\n'), /Create Private Threads/);
  assert.equal(rows.get('H1').priorEveryoneSend, SEND_BITS_NO_PRIVATE.toString(), 'only what can be given back is saved');
  assert.equal(guild.permissionCalls.length, 1);
  assert.equal(guild.permissionCalls[0].savedBefore, SEND_BITS_NO_PRIVATE.toString());
  assert.equal(everyoneBits(guild), VIEW | P.AddReactions | P.ReadMessageHistory | P.CreatePrivateThreads, 'the blocked bit is untouched');

  // The alert (and /antiraid status through permissionProblems) says it once.
  const embed = buildStartEmbed({ state: res.state, settings: S(), effects: res.effects, problems: permissionProblems(guild, S()) }).toJSON();
  assert.match(embed.fields.find((f) => f.name === 'Chat').value, /Partly locked/);
  const attention = embed.fields.find((f) => f.name === 'Needs attention').value;
  assert.match(attention, /Create Private Threads/);
  assert.equal(attention.split('\n').length, 1, 'the same words from both sources count once');

  // Ending gives back exactly what was taken: no 50013, nothing stuck.
  const end = await endLockdown({ guild }, d);
  assert.equal(end.restored, true);
  assert.equal(end.pendingRestore, false);
  assert.deepEqual(end.effects.errors, []);
  assert.equal(everyoneBits(guild), everyoneWithPrivate);
  assert.equal(rows.get('H1').priorEveryoneSend, null);
  assert.deepEqual(d.cancelled, [{ dedupeKey: 'antiraid.unlock:H1' }]);
});

test('a bot with Administrator takes every send bit, and one holding none takes nothing and says so', async () => {
  resetMemory();
  const admin = fakeGuild('H2', { perms: [...ALL_PERMS, 'Administrator'], everyone: everyoneWithPrivate, botBits: 0n });
  const r1 = await startLockdown({ guild: admin, settings: S({ antiRaidPauseInvites: false }) }, fakeDeps());
  assert.equal(r1.effects.chat, 'locked');
  assert.equal(rows.get('H2').priorEveryoneSend, (SEND_BITS_NO_PRIVATE | P.CreatePrivateThreads).toString());
  await endLockdown({ guild: admin }, fakeDeps());
  assert.equal(everyoneBits(admin), everyoneWithPrivate);

  const none = fakeGuild('H3', { everyone: everyoneWithPrivate, botBits: 0n });
  const r2 = await startLockdown({ guild: none, settings: S({ antiRaidPauseInvites: false }) }, fakeDeps());
  assert.equal(r2.started, true, 'the lockdown still stands');
  assert.equal(r2.effects.chat, 'failed');
  assert.match(r2.effects.errors.join('\n'), /Send Messages/);
  assert.equal(none.permissionCalls.length, 0, '@everyone is not touched');
  assert.equal(rows.get('H3').priorEveryoneSend ?? null, null, 'nothing to give back');
  assert.equal(everyoneBits(none), everyoneWithPrivate);
  const end = await endLockdown({ guild: none }, fakeDeps());
  assert.equal(end.pendingRestore, false);
  assert.equal(none.permissionCalls.length, 0);
});

test('a saved bit the bot cannot give back does not stop the others coming back, and is named for the next end', async () => {
  resetMemory();
  // Saved by an older lockdown (or the bot's role lost the permission meanwhile): all four bits, bot lacks Create Private Threads.
  const guild = fakeGuild('H4', { everyone: VIEW, botBits: SEND_BITS_NO_PRIVATE });
  rows.set('H4', { guildId: 'H4', lockedAt: null, lockedUntil: null, lockedById: null, reason: null, priorEveryoneSend: SEND_MASK.toString() });
  const d = fakeDeps();
  const res = await endLockdown({ guild }, d);
  assert.equal(res.pendingRestore, true);
  assert.equal(res.permanent, true, 'waiting does not help until the bot has the permission');
  assert.equal(res.restored, false);
  assert.equal(res.effects.chat, 'partialRestore');
  assert.match(res.effects.errors.join('\n'), /Create Private Threads/);
  assert.doesNotMatch(res.effects.errors.join('\n'), /Send Messages/, 'what came back is not reported as missing');
  assert.equal(everyoneBits(guild), VIEW | SEND_BITS_NO_PRIVATE, 'the rest of the send bits are back');
  assert.equal(rows.get('H4').priorEveryoneSend, P.CreatePrivateThreads.toString(), 'only the bit still owed stays saved');
  assert.deepEqual(d.cancelled, []);
  assert.match(buildEndEmbed({ how: 'manual', result: res }).toJSON().fields.find((f) => f.name === 'Chat').value, /partly/i);

  // Once a role of the bot has it, ending again finishes the job.
  guild.botBits = SEND_MASK;
  const retry = await endLockdown({ guild }, d);
  assert.equal(retry.restored, true);
  assert.equal(everyoneBits(guild), VIEW | SEND_MASK);
  assert.equal(rows.get('H4').priorEveryoneSend, null);
});

test('the unlock job says out loud when @everyone cannot be given back, even on a retry', async () => {
  resetMemory();
  const guild = fakeGuild('H5', { everyone: VIEW, botBits: 0n });
  settingsRows.set('H5', { guildId: 'H5', ...S() });
  rows.set('H5', { guildId: 'H5', lockedAt: null, lockedUntil: null, lockedById: null, reason: null, priorEveryoneSend: P.SendMessages.toString() });
  await assert.rejects(() => handleUnlockJob({ id: 'J' }, { guild }, fakeDeps()), PermanentJobError);
  const note = alerts(guild).at(-1);
  assert.ok(note, 'a note was posted although no lockdown was active any more');
  assert.match(fieldOf(note, 'Needs attention'), /Send Messages/);
});

test('a table read that was in flight when a raid tripped does not erase the lockdown memo', async () => {
  resetMemory();
  const guild = fakeGuild('M1');
  const realFind = prisma.antiRaidState.findUnique;
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  prisma.antiRaidState.findUnique = async (args) => { await gate; return realFind(args); }; // a slow read that finds no lockdown
  try {
    const slow = isLocked(guild);
    markActive(guild.id, Date.now() + 600_000); // the raid trips while the read is out
    release();
    assert.equal(await slow, true, 'the joiner who asked meets the lockdown');
    assert.equal(peekLocked('M1'), true, 'and the memo still says a lockdown is on');
  } finally {
    prisma.antiRaidState.findUnique = realFind;
  }
  // Without a lockdown in between, the "none" answer is still memoised.
  resetMemory();
  assert.equal(await isLocked(fakeGuild('M2')), false);
  assert.equal(peekLocked('M2'), false);
  // An active memo that has run out does not block a "none".
  markActive('M2', Date.now() - 1);
  assert.equal(await isLocked(fakeGuild('M2')), false);
  assert.equal(peekLocked('M2'), false);
});

test('the invite pause of a 24 hour lockdown is saved as set and an early end resumes it', async () => {
  resetMemory();
  const guild = fakeGuild('I1');
  const d = fakeDeps();
  const res = await startLockdown({ guild, settings: S({ antiRaidLockChat: false }), minutes: 1440 }, d);
  const row = rows.get('I1');
  const set = guild.incidentCalls[0].invitesDisabledUntil.getTime();
  assert.equal(res.effects.invites, 'paused');
  assert.equal(row.invitesPausedUntil.getTime(), set, 'the end the bot set is stored');
  assert.equal(res.state.invitesPausedUntil.getTime(), set);
  assert.ok(row.lockedUntil.getTime() - set > RAID.INVITE_MATCH_MS, 'capped further under lockedUntil than the match slack');

  const end = await endLockdown({ guild }, d);
  assert.equal(end.effects.invites, 'resumed');
  assert.equal(guild.incidentCalls.at(-1).invitesDisabledUntil, null);
  assert.equal(rows.get('I1').invitesPausedUntil, null, 'cleared after resuming');
});

test('an invite pause the bot did not set is never resumed, whatever its end', async () => {
  resetMemory();
  // A longer pause was already on: the bot sets none, so nothing is stored.
  const g1 = fakeGuild('I2');
  const longer = new Date(Date.now() + 40 * 60_000);
  g1.incidentsData = { invitesDisabledUntil: longer, dmsDisabledUntil: null };
  const d = fakeDeps();
  const r1 = await startLockdown({ guild: g1, settings: S({ antiRaidLockChat: false }) }, d);
  assert.equal(r1.effects.invites, 'already');
  assert.equal(rows.get('I2').invitesPausedUntil ?? null, null);
  // Even a pause ending at the lockdown's own end (the old comparison) is not ours.
  g1.incidentsData = { invitesDisabledUntil: rows.get('I2').lockedUntil, dmsDisabledUntil: null };
  const e1 = await endLockdown({ guild: g1 }, d);
  assert.equal(e1.effects.invites, 'foreign');
  assert.equal(g1.incidentCalls.length, 0, 'nothing was resumed');

  // The pause was changed by an admin after the bot set it.
  const g2 = fakeGuild('I3');
  await startLockdown({ guild: g2, settings: S({ antiRaidLockChat: false }) }, d);
  g2.incidentsData = { invitesDisabledUntil: new Date(Date.now() + 3 * 3_600_000), dmsDisabledUntil: null };
  assert.equal((await endLockdown({ guild: g2 }, d)).effects.invites, 'foreign');
  assert.equal(g2.incidentCalls.length, 1, 'only the pause at start');
  assert.equal(rows.get('I3').invitesPausedUntil, null);

  // Invites not paused by the setting: ending touches nothing.
  const g3 = fakeGuild('I4');
  await startLockdown({ guild: g3, settings: S({ antiRaidLockChat: false, antiRaidPauseInvites: false }) }, d);
  assert.equal((await endLockdown({ guild: g3 }, d)).effects.invites, 'none');
  assert.equal(g3.incidentCalls.length, 0);
});

// ── Joins ─────────────────────────────────────────────────────────────────────

test('a join flood starts one lockdown with an alert and the joiners are kicked', async () => {
  resetMemory();
  const guild = fakeGuild('J1');
  const d = fakeDeps();
  const settings = S();
  const outs = [];
  for (let i = 0; i < 2; i++) outs.push(await handleMemberAdd(ctxFor(guild, fakeMember(guild), settings), d));
  assert.deepEqual(outs.map((o) => o.action), [null, null], 'below the threshold nothing happens');
  assert.equal(d.mod.length, 0);

  const third = fakeMember(guild);
  const tripCtx = ctxFor(guild, third, settings);
  const trip = await handleMemberAdd(tripCtx, d);
  assert.equal(trip.raid, true);
  const started = await trip.start;
  assert.equal(started.started, true);
  assert.equal(trip.action, 'KICK', 'the member whose join tripped it is part of the raid');
  assert.equal(tripCtx.state.antiraid.action, 'KICK');

  const alert = alerts(guild);
  assert.equal(alert.length, 1);
  assert.match(embedOf(alert[0]).description, /\*\*3\*\* members joined within \*\*10\*\* seconds/);
  assert.equal(alert[0].components[0].toJSON().components[0].custom_id, 'antiraid:end');
  assert.ok(rows.get('J1').lockedAt);

  const fourth = fakeMember(guild);
  const later = await handleMemberAdd(ctxFor(guild, fourth, settings), d);
  assert.equal(later.raid, false);
  assert.equal(later.action, 'KICK');
  assert.equal(alerts(guild).length, 1, 'one alert per lockdown, not per joiner');

  assert.equal(d.mod.length, 2);
  const call = d.mod[1];
  assert.equal(call.action, 'KICK');
  assert.equal(call.target, fourth);
  assert.equal(call.moderator, guild.client.user);
  assert.equal(call.source, moderationSource());
  assert.equal(call.silent, true, 'no DM to a raider');
  assert.equal(call.skipEscalation, true);
  assert.match(call.reason, /^Anti-Raid: raid lockdown/);
  assert.equal('deleteMessageSeconds' in call, false, 'a kick deletes no messages');
  assert.match(call.logFields[0].value, /raid lockdown/i);
  assert.equal(svc.tallyOf('J1').kicked, 2);
});

test('a burst of joins at once starts exactly one lockdown', async () => {
  resetMemory();
  const guild = fakeGuild('J2');
  const d = fakeDeps();
  const outs = await Promise.all(Array.from({ length: 8 }, () => handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d)));
  await Promise.all(outs.map((o) => o?.start));
  assert.equal(outs.filter((o) => o.raid).length, 1);
  assert.equal(d.jobs.length, 1);
  assert.equal(guild.permissionCalls.length, 1);
  assert.equal(alerts(guild).length, 1);
  assert.equal(d.mod.length, 6, 'the joiner that tripped it and the five after it');
});

test('bots are never counted or actioned', async () => {
  resetMemory();
  const guild = fakeGuild('J3');
  const d = fakeDeps();
  const hook = plugin.hooks.find((h) => h.event === 'guildMemberAdd');
  const bot = fakeMember(guild, { bot: true });
  assert.equal(hook.filter({ member: bot }), false);
  assert.equal(hook.filter({ member: fakeMember(guild) }), true);
  for (let i = 0; i < 6; i++) assert.equal(await handleMemberAdd(ctxFor(guild, fakeMember(guild, { bot: true }), S()), d), null);
  assert.equal(d.jobs.length, 0, 'bots do not fill the window');
  // During a lockdown a bot still gets nothing.
  await startLockdown({ guild, settings: S() }, d);
  assert.equal(await handleMemberAdd(ctxFor(guild, fakeMember(guild, { bot: true }), S()), d), null);
  assert.equal(d.mod.length, 0);
});

test('ALERT only reports: nobody is kicked or banned', async () => {
  resetMemory();
  const guild = fakeGuild('J4');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S({ antiRaidAction: 'ALERT' }) }, d);
  const out = await handleMemberAdd(ctxFor(guild, fakeMember(guild), S({ antiRaidAction: 'ALERT' })), d);
  assert.equal(out.action, 'ALERT');
  assert.equal(d.mod.length, 0);
  assert.equal(svc.tallyOf('J4').flagged, 1);
});

test('a BAN lockdown bans and clears the raider\'s last hour of messages', async () => {
  resetMemory();
  const guild = fakeGuild('J5');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S({ antiRaidAction: 'BAN' }) }, d);
  await handleMemberAdd(ctxFor(guild, fakeMember(guild), S({ antiRaidAction: 'BAN' })), d);
  assert.equal(d.mod[0].action, 'BAN');
  assert.equal(d.mod[0].deleteMessageSeconds, 3600);
  assert.equal(svc.tallyOf('J5').banned, 1);
});

test('failures are counted, staff and absent members are not', async () => {
  resetMemory();
  const guild = fakeGuild('J6');
  const d = fakeDeps();
  await startLockdown({ guild, settings: S() }, d);
  d.modResult = { ok: false, code: 'IMMUNE', error: 'immune' };
  await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d);
  d.modResult = { ok: false, code: 'NOT_IN_GUILD', error: 'gone' };
  await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d);
  assert.equal(svc.tallyOf('J6').failed, 0);
  d.modResult = { ok: false, code: 'HIERARCHY', error: 'my role is too low' };
  await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d);
  assert.equal(svc.tallyOf('J6').failed, 1);
  d.executeModAction = async () => { throw new Error('boom'); };
  assert.ok(await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d), 'a throwing action never escapes');
});

test('young accounts get their own action with no lockdown, and the alert is throttled', async () => {
  resetMemory();
  const guild = fakeGuild('Y1');
  const d = fakeDeps();
  // A high threshold: these joins are young accounts, not a flood.
  const settings = S({ antiRaidMinAccountAgeDays: 7, antiRaidYoungAction: 'BAN', antiRaidJoinThreshold: 50 });
  const young = fakeMember(guild, { created: Date.now() - 2 * 86_400_000 });
  const out = await handleMemberAdd(ctxFor(guild, young, settings), d);
  assert.equal(out.action, 'BAN');
  assert.equal(d.mod.length, 1);
  assert.equal(d.mod[0].action, 'BAN');
  assert.match(d.mod[0].reason, /^Anti-Raid: account is 2 days old \(minimum 7\)$/);
  assert.equal(d.mod[0].silent, true);
  assert.equal(alerts(guild).length, 1);
  assert.equal(fieldOf(alerts(guild)[0], 'Action'), 'Banned');
  assert.equal(rows.get('Y1') ?? null, null, 'no lockdown for a single young account');

  await handleMemberAdd(ctxFor(guild, fakeMember(guild, { created: Date.now() - 86_400_000 }), settings), d);
  await handleMemberAdd(ctxFor(guild, fakeMember(guild, { created: Date.now() - 86_400_000 }), settings), d);
  assert.equal(d.mod.length, 3, 'every young account is still actioned');
  assert.equal(alerts(guild).length, 1, 'but only one alert per minute');

  // Old enough: nothing.
  const before = d.mod.length;
  assert.equal((await handleMemberAdd(ctxFor(guild, fakeMember(guild), settings), d)).action, null);
  assert.equal(d.mod.length, before);
});

test('young ALERT only reports', async () => {
  resetMemory();
  const guild = fakeGuild('Y2');
  const d = fakeDeps();
  const settings = S({ antiRaidMinAccountAgeDays: 30, antiRaidYoungAction: 'ALERT' });
  await handleMemberAdd(ctxFor(guild, fakeMember(guild, { created: Date.now() - 3 * 86_400_000 }), settings), d);
  assert.equal(d.mod.length, 0);
  assert.equal(alerts(guild).length, 1);
  assert.equal(fieldOf(alerts(guild)[0], 'Action'), 'Only reported');
});

test('a member who is both a raider and too young is actioned once, with the harsher action', async () => {
  resetMemory();
  const d = fakeDeps();
  const youngJoin = (guild, settings) => fakeMember(guild, { created: Date.now() - 86_400_000 });

  const g1 = fakeGuild('Y3');
  const s1 = S({ antiRaidAction: 'KICK', antiRaidMinAccountAgeDays: 7, antiRaidYoungAction: 'BAN' });
  await startLockdown({ guild: g1, settings: s1 }, d);
  await handleMemberAdd(ctxFor(g1, youngJoin(g1), s1), d);
  assert.equal(d.mod.length, 1);
  assert.equal(d.mod[0].action, 'BAN');
  assert.doesNotMatch(d.mod[0].reason, /raid lockdown/, 'the reason names what decided it');

  const g2 = fakeGuild('Y4');
  const s2 = S({ antiRaidAction: 'BAN', antiRaidMinAccountAgeDays: 7, antiRaidYoungAction: 'KICK' });
  await startLockdown({ guild: g2, settings: s2 }, d);
  await handleMemberAdd(ctxFor(g2, youngJoin(g2), s2), d);
  assert.equal(d.mod.length, 2);
  assert.equal(d.mod[1].action, 'BAN');
  assert.match(d.mod[1].reason, /raid lockdown/);
  assert.doesNotMatch(d.mod[1].reason, /days old/);

  const g3 = fakeGuild('Y5');
  const s3 = S({ antiRaidAction: 'KICK', antiRaidMinAccountAgeDays: 7, antiRaidYoungAction: 'KICK' });
  await startLockdown({ guild: g3, settings: s3 }, d);
  await handleMemberAdd(ctxFor(g3, youngJoin(g3), s3), d);
  assert.equal(d.mod.length, 3);
  assert.match(d.mod[2].reason, /raid lockdown; account is 1 day old/);
});

test('a lockdown that is on in the table is found after a restart', async () => {
  resetMemory();
  const guild = fakeGuild('R1');
  const d = fakeDeps();
  rows.set('R1', { guildId: 'R1', lockedAt: new Date(), lockedUntil: new Date(Date.now() + 600_000), lockedById: null, reason: null, priorEveryoneSend: null });
  const out = await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d);
  assert.equal(out.raid, false);
  assert.equal(out.action, 'KICK', 'the joiner meets the lockdown the table remembers');
  assert.equal(d.jobs.length, 0, 'and no new lockdown is started');
});

test('a lockdown that ran out without being ended is ended by the next join', async () => {
  resetMemory();
  const guild = fakeGuild('R2');
  const d = fakeDeps();
  rows.set('R2', { guildId: 'R2', lockedAt: new Date(Date.now() - 7_200_000), lockedUntil: new Date(Date.now() - 60_000), lockedById: null, reason: null, priorEveryoneSend: null });
  settingsRows.set('R2', { guildId: 'R2', ...S() });
  const out = await handleMemberAdd(ctxFor(guild, fakeMember(guild), S()), d);
  assert.equal(out.action, null, 'an expired lockdown does not action anyone');
  for (let i = 0; i < 20 && rows.get('R2').lockedAt; i++) await new Promise((r) => { setTimeout(r, 5); });
  assert.equal(rows.get('R2').lockedAt, null, 'the self-heal ended it');
});

// ── Unlock job and End button ─────────────────────────────────────────────────

test('the unlock job ends a due lockdown and announces it', async () => {
  resetMemory();
  const guild = fakeGuild('U1');
  const d = fakeDeps();
  settingsRows.set('U1', { guildId: 'U1', ...S() });
  await startLockdown({ guild, settings: S() }, d);
  // Time passes: the lockdown and the invite pause (which Discord lifts itself) both run out.
  rows.set('U1', { ...rows.get('U1'), lockedUntil: new Date(Date.now() - 1000) });
  guild.incidentsData = { invitesDisabledUntil: new Date(Date.now() - 1000), dmsDisabledUntil: null };
  await handleUnlockJob({ id: 'J' }, { guild }, d);
  assert.equal(rows.get('U1').lockedAt, null);
  assert.equal(everyoneBits(guild), EVERYONE);
  assert.equal(guild.incidentCalls.length, 1, 'a pause that is over needs no call');
  const note = alerts(guild).at(-1);
  assert.match(embedOf(note).title, /ended/);
  assert.match(embedOf(note).description, /ran out/);

  // Run again (a retry): nothing to do, nothing posted.
  const posted = alerts(guild).length;
  await handleUnlockJob({ id: 'J' }, { guild }, d);
  assert.equal(alerts(guild).length, posted);
  await handleUnlockJob({ id: 'J' }, { guild: null }, d); // no guild: no-op
});

test('the unlock job leaves a lockdown that is not due and works while the module is off', async () => {
  resetMemory();
  const guild = fakeGuild('U2');
  const d = fakeDeps();
  settingsRows.set('U2', { guildId: 'U2', ...S({ antiRaidEnabled: false }) });
  await startLockdown({ guild, settings: S() }, d);
  await handleUnlockJob({ id: 'J' }, { guild }, d);
  assert.ok(rows.get('U2').lockedAt, 'started again: the old job does not end the new one');
  rows.set('U2', { ...rows.get('U2'), lockedUntil: new Date(Date.now() - 1000) });
  await handleUnlockJob({ id: 'J' }, { guild }, d);
  assert.equal(rows.get('U2').lockedAt, null, 'ended although the module is off');
});

test('the unlock job fails (to be retried) when @everyone cannot be restored, permanently without permission', async () => {
  resetMemory();
  const guild = fakeGuild('U3');
  const d = fakeDeps();
  settingsRows.set('U3', { guildId: 'U3', ...S() });
  await startLockdown({ guild, settings: S() }, d);
  rows.set('U3', { ...rows.get('U3'), lockedUntil: new Date(Date.now() - 1000) });
  guild.failEveryone = Object.assign(new Error('timeout'), { code: 500 });
  await assert.rejects(() => handleUnlockJob({ id: 'J' }, { guild }, d), (err) => !(err instanceof PermanentJobError) && /Send Messages/.test(err.message));
  assert.equal(rows.get('U3').lockedAt, null);
  assert.ok(rows.get('U3').priorEveryoneSend, 'the bits wait for the retry');
  guild.failEveryone = null;
  await handleUnlockJob({ id: 'J' }, { guild }, d);
  assert.equal(everyoneBits(guild), EVERYONE);
  assert.equal(rows.get('U3').priorEveryoneSend, null);

  const g2 = fakeGuild('U4');
  settingsRows.set('U4', { guildId: 'U4', ...S() });
  await startLockdown({ guild: g2, settings: S() }, d);
  rows.set('U4', { ...rows.get('U4'), lockedUntil: new Date(Date.now() - 1000) });
  g2.failEveryone = Object.assign(new Error('Missing Permissions'), { code: 50013 });
  await assert.rejects(() => handleUnlockJob({ id: 'J' }, { guild: g2 }, d), PermanentJobError);
});

function fakeButton(guild, { perms = ['KickMembers'], userId = 'MOD' } = {}) {
  const i = {
    customId: 'antiraid:end', guild, guildId: guild.id, user: { id: userId }, createdTimestamp: Date.now(),
    member: { id: userId, guild, permissions: new PermissionsBitField(perms), roles: { cache: new Map() } },
    deferred: false, replied: false, ephemeral: null, replies: [], edits: [], followUps: [],
    inCachedGuild: () => true,
    async reply(p) { this.replied = true; this.replies.push(p); return { resource: { message: {} } }; },
    async deferUpdate() { this.deferred = true; },
    async editReply(p) { this.edits.push(p); return {}; },
    async followUp(p) { this.followUps.push(p); return {}; },
  };
  return i;
}

test('the End button: moderators end the lockdown, members are refused, a stale click is harmless', async () => {
  resetMemory();
  const guild = fakeGuild('B1');
  const d = fakeDeps();
  settingsRows.set('B1', { guildId: 'B1', ...S({ antiRaidEnabled: false }) }); // module off: the button still works
  await startLockdown({ guild, settings: S() }, d);

  const member = fakeButton(guild, { perms: [] });
  await handleEndButton(member, null, d);
  assert.ok(rows.get('B1').lockedAt, 'a member cannot end it');
  assert.equal(member.deferred, false);
  assert.match(embedOf(member.replies[0]).description, /Only moderators/);
  assert.equal(member.replies[0].ephemeral, true);

  const mod = fakeButton(guild);
  await handleEndButton(mod, null, d);
  assert.equal(rows.get('B1').lockedAt, null);
  assert.equal(everyoneBits(guild), EVERYONE);
  assert.equal(mod.deferred, true);
  assert.deepEqual(mod.edits, [{ components: [] }], 'the button is removed from the alert');
  assert.match(embedOf(mod.followUps[0]).title, /Lockdown Ended/);
  assert.equal(mod.followUps[0].ephemeral, true);
  assert.match(embedOf(alerts(guild).at(-1)).description, /Ended by <@MOD>/);

  const stale = fakeButton(guild);
  await handleEndButton(stale, null, d);
  assert.match(embedOf(stale.followUps[0]).title, /No Lockdown/);

  const bad = fakeButton(guild);
  bad.customId = 'antiraid:other';
  await handleEndButton(bad, null, d);
  assert.match(bad.replies[0].content, /no longer active/);
});

test('an admin by role or Discord permission passes the moderator check', async () => {
  resetMemory();
  const guild = fakeGuild('B2');
  settingsRows.set('B2', { guildId: 'B2', ...S(), modRoleIds: ['MODROLE'] });
  await startLockdown({ guild, settings: S() }, fakeDeps());
  const byRole = fakeButton(guild, { perms: [] });
  byRole.member.roles.cache.set('MODROLE', {});
  await handleEndButton(byRole, null, fakeDeps());
  assert.equal(rows.get('B2').lockedAt, null);
});

// ── Channel delete ────────────────────────────────────────────────────────────

test('deleting the alert channel clears it from the settings', async () => {
  settingsRows.set('D1', { guildId: 'D1', ...S({ antiRaidAlertChannelId: 'C9' }) });
  const ctx = (channelId, settings) => ({ guildId: 'D1', channel: { id: channelId }, getSettings: async () => settings });
  await onChannelDelete(ctx('OTHER', S({ antiRaidAlertChannelId: 'C9' })));
  assert.equal(settingsRows.get('D1').antiRaidAlertChannelId, 'C9', 'another channel changes nothing');
  await onChannelDelete(ctx('C9', S({ antiRaidAlertChannelId: 'C9' })));
  assert.equal(settingsRows.get('D1').antiRaidAlertChannelId, null);
  assert.equal(mayBeAlertChannel({ guildId: 'NO-MEMO', channel: { id: 'X' } }), true, 'without a memo the handler decides');
});

// ── Declarations ──────────────────────────────────────────────────────────────

test('plugin declaration, hook order and component gating', async () => {
  assert.equal(plugin.id, 'antiraid');
  assert.equal(plugin.module, 'antiraid');
  assert.deepEqual(plugin.hooks.map((h) => [h.event, h.id, h.always === true]), [['guildMemberAdd', 'antiraid', false], ['channelDelete', 'antiraid', true]]);
  const order = HOOK_ORDER.guildMemberAdd;
  assert.ok(order.indexOf('antiraid') > order.indexOf('moderation'), 'after moderation');
  for (const later of ['logging', 'captcha', 'welcome']) assert.ok(order.indexOf('antiraid') < order.indexOf(later), `before ${later}`);
  assert.ok(HOOK_ORDER.channelDelete.includes('antiraid'));
  assert.deepEqual(plugin.components.map((c) => [c.type, c.prefix, c.module]), [['button', 'antiraid', null]], 'ending works with the module off');
  assert.deepEqual(plugin.jobs.map((j) => j.type), ['antiraid.unlock']);
  const { plugins, errors } = await collectPlugins();
  assert.ok(plugins.some((p) => p.id === 'antiraid'), 'collectPlugins keeps it');
  assert.deepEqual(errors.filter((e) => /antiraid/.test(e)), []);
});

test('/antiraid payload', () => {
  const json = buildCommandPayload(cmd);
  const charCount = JSON.stringify(json).length;
  assert.ok(charCount <= 4000, `payload is ${charCount} chars`);
  assert.equal(json.name, 'antiraid');
  assert.deepEqual(json.options.map((o) => o.name), ['status', 'lockdown', 'config']);
  const lockdown = json.options.find((o) => o.name === 'lockdown');
  assert.equal(lockdown.type, 2, 'a subcommand group');
  assert.deepEqual(lockdown.options.map((o) => o.name), ['start', 'end']);
  const config = json.options.find((o) => o.name === 'config');
  const range = (name) => config.options.find((o) => o.name === name);
  assert.deepEqual([range('joins').min_value, range('joins').max_value], [3, 100]);
  assert.deepEqual([range('seconds').min_value, range('seconds').max_value], [3, 300]);
  assert.deepEqual([range('minutes').min_value, range('minutes').max_value], [5, 1440]);
  assert.deepEqual([range('min_account_age').min_value, range('min_account_age').max_value], [0, 365]);
  assert.deepEqual(range('action').choices.map((c) => c.value), ['ALERT', 'KICK', 'BAN']);
  assert.equal(cmd.module, 'antiraid');
  assert.equal(cmd.gated, false);
  assert.equal(cmd.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(BigInt(json.default_member_permissions), P.ManageGuild);
});

// ── /antiraid replies ─────────────────────────────────────────────────────────

function fakeCommand(guild, { group = null, sub, options = {}, channelId = 'C-HERE' }) {
  const get = (n) => (Object.hasOwn(options, n) ? options[n] : null);
  return {
    guild, guildId: guild.id, channelId, user: { id: 'ADMIN' },
    options: { getSubcommandGroup: () => group, getSubcommand: () => sub, getString: get, getInteger: get, getBoolean: get, getChannel: get },
    deferred: false, replied: false, ephemeral: null, replies: [],
    async deferReply() { this.deferred = true; this.ephemeral = false; },
    async editReply(p) { this.replies.push(p); return {}; },
  };
}
const lastEmbed = (i) => embedOf(i.replies.at(-1));

test('/antiraid config shows, changes and warns', async () => {
  resetMemory();
  const guild = fakeGuild('C1', { perms: ['ManageGuild'] });
  settingsRows.set('C1', { guildId: 'C1', ...S({ antiRaidEnabled: false, antiRaidAction: 'ALERT', antiRaidLockChat: false }) });

  const show = fakeCommand(guild, { sub: 'config' });
  await cmd.execute(show);
  assert.match(lastEmbed(show).description, /Detection:\*\* 3 joins within 10 seconds/);

  const set = fakeCommand(guild, {
    sub: 'config',
    options: { joins: 8, seconds: 20, action: 'BAN', lock_chat: true, min_account_age: 3, young_action: 'KICK', enabled: true },
  });
  await cmd.execute(set);
  const row = settingsRows.get('C1');
  assert.deepEqual(
    [row.antiRaidJoinThreshold, row.antiRaidJoinSeconds, row.antiRaidAction, row.antiRaidLockChat, row.antiRaidMinAccountAgeDays, row.antiRaidYoungAction, row.antiRaidEnabled],
    [8, 20, 'BAN', true, 3, 'KICK', true]);
  assert.match(lastEmbed(set).title, /Anti-Raid Updated/);
  assert.match(lastEmbed(set).description, /Locking chat: I need \*\*Manage Roles\*\*/);
  assert.match(lastEmbed(set).description, /Banning members: I need \*\*Ban Members\*\*/);
  assert.doesNotMatch(lastEmbed(set).description, /module is off/);

  const off = fakeCommand(guild, { sub: 'config', options: { enabled: false } });
  await cmd.execute(off);
  assert.match(lastEmbed(off).description, /module is off/);

  const badChannel = fakeCommand(guild, { sub: 'config', options: { alert_channel: { id: 'NOPE' } } });
  await cmd.execute(badChannel);
  assert.match(lastEmbed(badChannel).title, /Cannot Use That Channel/);

  const okChannel = fakeCommand(guild, { sub: 'config', options: { alert_channel: { id: 'ALERT' } } });
  await cmd.execute(okChannel);
  assert.equal(settingsRows.get('C1').antiRaidAlertChannelId, 'ALERT');
});

test('/antiraid lockdown start and end, with the module off', async () => {
  resetMemory();
  const guild = fakeGuild('C2');
  settingsRows.set('C2', { guildId: 'C2', ...S({ antiRaidEnabled: false }) });
  // The command uses the real scheduler: stub its two tables (the deps seam is for the service functions).
  const realJobs = prisma.scheduledJob;
  prisma.scheduledJob = { deleteMany: async () => ({ count: 1 }) };
  prisma.$transaction = async () => ({ id: 'J', runAt: new Date() });
  try {
    const start = fakeCommand(guild, { group: 'lockdown', sub: 'start', options: { minutes: 15, reason: 'spam wave' } });
    await cmd.execute(start);
    assert.equal(start.replies.length, 1);
    const embed = lastEmbed(start);
    assert.match(embed.title, /Lockdown started/);
    assert.match(embed.description, /Started by <@ADMIN>: spam wave/);
    assert.match(start.replies[0].content, /module is off/, 'a manual start while off says joiners are not actioned');
    assert.equal(rows.get('C2').lockedById, 'ADMIN');
    assert.equal(alerts(guild).length, 1, 'the alert with the button went to the alert channel');
    assert.equal(rows.get('C2').lockedUntil.getTime() - rows.get('C2').lockedAt.getTime(), 15 * 60_000);

    const again = fakeCommand(guild, { group: 'lockdown', sub: 'start' });
    await cmd.execute(again);
    assert.match(lastEmbed(again).title, /Already Locked Down/);

    const status = fakeCommand(guild, { sub: 'status' });
    await cmd.execute(status);
    assert.match(fieldOf(status.replies[0], 'Lockdown'), /^🔒 On until/);
    assert.match(fieldOf(status.replies[0], 'Detection'), /Off/);

    const end = fakeCommand(guild, { group: 'lockdown', sub: 'end' });
    await cmd.execute(end);
    assert.match(lastEmbed(end).title, /ended/);
    assert.equal(rows.get('C2').lockedAt, null);
    assert.equal(everyoneBits(guild), EVERYONE);

    const none = fakeCommand(guild, { group: 'lockdown', sub: 'end' });
    await cmd.execute(none);
    assert.match(lastEmbed(none).title, /No Lockdown/);
    const after = fakeCommand(guild, { sub: 'status' });
    await cmd.execute(after);
    assert.equal(fieldOf(after.replies[0], 'Lockdown'), '🔓 Off');
  } finally {
    prisma.scheduledJob = realJobs;
    delete prisma.$transaction;
  }
});
