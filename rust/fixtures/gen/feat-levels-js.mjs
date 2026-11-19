// The JS side of feat-levels' tests: the JS bot's own xpService (and Prisma) against the test servers
// (TEST_DATABASE_URL, TEST_REDIS_URL). Prints one `RESULT <json>` line.
//
//   node rust/fixtures/gen/feat-levels-js.mjs golden                        writes rust/fixtures/golden/feat-levels/pure.json (no servers)
//   node rust/fixtures/gen/feat-levels-js.mjs apply <g> <u> <delta> [messages] [voiceMinutes] [touch]   applyXpDelta
//   node rust/fixtures/gen/feat-levels-js.mjs setxp <g> <u> <xp>            setXp
//   node rust/fixtures/gen/feat-levels-js.mjs give <g> <json userIds> <amount>   giveXpToMembers
//   node rust/fixtures/gen/feat-levels-js.mjs rank <g> <u> <xp>             getRank
//   node rust/fixtures/gen/feat-levels-js.mjs load <g> <u>                  prisma.userLevel.findUnique
//   node rust/fixtures/gen/feat-levels-js.mjs rewards <g>                   prisma.levelRole.findMany (level asc)
//   node rust/fixtures/gen/feat-levels-js.mjs addreward <g> <level> <roleId>   the /levelconfig role add upsert
//   node rust/fixtures/gen/feat-levels-js.mjs settings <g>                  the levels fields of getSettings
//   node rust/fixtures/gen/feat-levels-js.mjs boost <g> <json list>         updateSettingsWith xpBoostRoles = normalizeBoostRoles(list)
//   node rust/fixtures/gen/feat-levels-js.mjs roledelete <g> <roleId>       handleRoleDelete
//   node rust/fixtures/gen/feat-levels-js.mjs seedvoice <g> <channel> <u1> <u2>   seedVoiceSessions with two members in one channel
//   node rust/fixtures/gen/feat-levels-js.mjs voiceget <g> <u>              parseVoiceSession(await cache.get(voice key)) and the raw value
//   node rust/fixtures/gen/feat-levels-js.mjs job <dedupeKey>               the ScheduledJob row
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const imp = (...p) => import(pathToFileURL(join(ROOT, ...p)).href);
process.env.NODE_ENV = 'production';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? 'postgresql://offline:offline@127.0.0.1:1/offline';
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';

const answer = (value) => process.stdout.write(`\nRESULT ${JSON.stringify(value)}\n`);

const svc = await imp('src', 'services', 'xpService.js');
const levelconfig = await imp('src', 'commands', 'admin', 'levelconfig.js');
const settingsSvc = await imp('src', 'services', 'settingsService.js');
const { default: prisma } = await imp('src', 'database', 'client.js');
const { default: redis, cache } = await imp('src', 'services', 'redis.js');
const { ChannelType } = await import('discord.js');

const noTs = (j) => { const c = JSON.parse(JSON.stringify(j)); delete c.timestamp; return c; };
const ej = (e) => noTs(typeof e?.toJSON === 'function' ? e.toJSON() : e);

// ── Offline golden values ──────────────────────────────────────────────────────

async function golden() {
  const out = {};
  const xs = [0, 1, 99, 100, 101, 254, 255, 1149, 1150, 1200, 4675, 4674, 1_899_250, 1_000_000, 1_000_000_000, 2_147_483_647, -5, 12.5];
  out.levels = xs.map((xp) => ({ xp, level: svc.levelFromXp(xp), into: svc.xpIntoLevel(xp) }));
  out.totals = [-3, 0, 1, 2, 5, 10, 100, 500, 838, 839, 900, 2.7].map((level) => ({ level, total: svc.totalXpForLevel(level) }));

  const ranges = [[null, null], [30.9, 10], [0, 0], [5, 5000], [15, 25], [null, 7], [-4, 2000], [1000, 1], [7.9, 7.1]];
  out.ranges = ranges.map(([min, max]) => ({ min, max, out: svc.normalizeXpRange(min ?? 'x', max ?? undefined) }));
  const rolls = [];
  for (const [min, max, multiplier, boost] of [[15, 25, 1, 1], [25, 15, 1, 1], [15, 25, 1.5, 2], [15, 25, 0, 1], [15, 25, 1, 0.5], [0, 0, 1, 1], [5, 5000, 1, 1], [10, 20, 0.1, 1], [1, 1, 2.5, 1.37]]) {
    for (const r of [0, 0.25, 0.5, 0.99999]) rolls.push({ min, max, multiplier, boost, r, xp: svc.rollXp({ min, max, multiplier, boost }, () => r) });
  }
  out.rolls = rolls;

  const R = (n) => `95500920000000${String(n).padStart(4, '0')}`;
  const boostInputs = [
    null, { roleId: R(1), multiplier: 2 },
    [{ roleId: R(1), multiplier: 1.5 }, { roleId: 'abc', multiplier: 2 }, { roleId: R(2), multiplier: 9 }, { roleId: R(3), multiplier: 'x' },
      { roleId: R(4), multiplier: 1.234 }, { roleId: R(1), multiplier: 3 }, null, 'x', { roleId: R(5), multiplier: 1.005 }, { roleId: R(6), multiplier: 4.999 }],
    [{ roleId: R(5), multiplier: 2.5 }, { roleId: R(5), multiplier: 1.5 }],
    Array.from({ length: 15 }, (_, i) => ({ roleId: R(100 + i), multiplier: 2 })),
    [{ roleId: R(7), multiplier: 1 }, { roleId: R(8), multiplier: 5 }, { roleId: R(9), multiplier: 0.99 }, { roleId: '1234567890123456', multiplier: 2 }],
  ];
  out.boosts = boostInputs.map((input) => ({ input, out: svc.normalizeBoostRoles(input) }));

  const rewards = [{ level: 1, roleId: 'A' }, { level: 5, roleId: 'B' }, { level: 10, roleId: 'C' }];
  const dup = [{ level: 10, roleId: 'A' }, { level: 1, roleId: 'A' }, { level: 5, roleId: 'B' }, { level: 5, roleId: 'D' }];
  const plans = [];
  for (const list of [rewards, dup]) {
    for (const level of [0, 1, 3, 4, 5, 9, 10, 50]) {
      for (const stack of [true, false]) {
        for (const syncDown of [true, false]) {
          for (const held of [[], ['A'], ['B', 'C'], ['A', 'B', 'C'], ['D', 'C']]) {
            plans.push({ rewards: list, level, stack, syncDown, held, out: svc.planLevelRoles({ rewards: list, level, stack, syncDown, memberRoleIds: held }) });
          }
        }
      }
    }
  }
  out.plans = plans;

  const targets = [];
  for (const mode of ['OFF', 'CURRENT', 'DM', 'CHANNEL', 'WHATEVER', null]) {
    for (const source of ['message', 'voice', 'reward', 'admin']) {
      for (const [cfg, cur] of [['C', 'S'], [null, 'S'], ['C', null], [null, null]]) {
        targets.push({ mode, source, cfg, cur, out: svc.resolveAnnounceTarget({ mode: mode ?? undefined, source, levelUpChannelId: cfg, sourceChannelId: cur }) });
      }
    }
  }
  out.targets = targets;

  const now = 1_700_000_000_000;
  out.segments = [
    [now - 125_000, now, 0], [now - 125_000, now, now - 61_000], [now - 30 * 3_600_000, now, 0], [now + 5000, now, 0], [now - 30_000, now, 0],
    [now - 24 * 3_600_000, now, 0], [now - 24 * 3_600_000 + 1, now, 0], [now - 600_000, now, now + 1000], [now - 59_999, now, 0],
  ].map(([start, n, readyAt]) => ({ start, now: n, readyAt, out: svc.computeVoiceSegment({ start, now: n, readyAt }) }));
  out.voiceXp = [[0, 5, 1, 1], [10, 5, 1, 1], [2, 500, 1, 1], [10, 5, 2, 1.5], [1, 1, 0.1, 1], [3, null, 1, 1], [1440, 50, 10, 5], [0.5, 5, 1, 1], [7, 0, 1, 1], [5, 3.7, 1.25, 1.1]]
    .map(([minutes, rate, multiplier, boost]) => ({ minutes, rate, multiplier, boost, xp: svc.voiceXpForMinutes(minutes, { rate: rate ?? undefined, multiplier, boost }) }));
  const raws = [1_700_000_000_000, '1700000000000', { start: 1_700_000_000_000, channelId: 'V' }, 'abc', 0, now + 61_000, null, { start: 'x' },
    now + 30_000, { start: '1699999999000', channelId: '' }, { start: 1_699_999_999_000.5, channelId: 5 }, ' 1699999999999 ', [1], true, -5, { channelId: 'V' }, '', '12e3'];
  out.sessions = raws.map((raw) => ({ raw, out: svc.parseVoiceSession(raw, now + 100_000) }));
  out.expiring = ['https://cdn.discordapp.com/attachments/1/2/a.png?ex=1', 'https://media.discordapp.net/ephemeral-attachments/1/2/a.png', 'https://CDN.discordapp.com/Attachments/1/2/a.png',
    'https://cdn.discordapp.com/avatars/1/a.png', 'https://example.com/attachments/a.png', 'not a url', 'https://cdn.discordapp.com.evil.com/attachments/a.png',
    'http://media.discordapp.net/attachments/x', 'https://cdn.discordapp.com/emojis/1.png'].map((url) => ({ url, out: levelconfig.isExpiringDiscordLink(url) }));
  out.mentions = ['GG {player}', '{PLAYER}', '{Mention}', '{user}', '{user.mention}', '{user.name}', 'level {level}', '{ player }', ''].map((t) => ({ t, out: svc.templateMentionsPlayer(t) }));

  // buildLevelUpPayload, with the same render context bot-core's fixtures use.
  const IMAGE = (base) => (opts) => `${base}.${opts?.extension}?size=${opts?.size}`;
  const userA = { id: '111111111111111111', username: 'alice', tag: 'alice', discriminator: '0', globalName: 'Alice', bot: false,
    createdTimestamp: 1_600_000_000_123, displayAvatarURL: IMAGE('https://cdn/u/a') };
  const guildA = { id: '999', name: 'My Server', ownerId: '111111111111111111', memberCount: 1234567, verificationLevel: 2,
    joinedTimestamp: 1_650_000_000_000, createdTimestamp: 1_500_000_000_000, premiumSubscriptionCount: 14, premiumTier: 3, iconURL: IMAGE('https://cdn/g') };
  const memberA = { user: userA, guild: guildA, displayName: 'A*li_', joinedTimestamp: 1_700_000_000_500, displayAvatarURL: IMAGE('https://cdn/m/a') };
  const channelA = { id: '500', name: 'general', type: ChannelType.GuildText };
  out.renderContext = { userA: { ...userA, displayAvatarURL: 'https://cdn/u/a.png?size=256' }, guildA: { ...guildA, iconURL: 'https://cdn/g.png?size=256' },
    memberA: { displayName: 'A*li_', joinedTimestamp: 1_700_000_000_500, avatarURL: 'https://cdn/m/a.png?size=256' }, channelA };
  const templates = ['GG {player}, you just advanced to level {level}!', 'GG {player}! {player} hit {level} in {server} $& {player.name} {PLAYER}',
    '{user} {mention} #{rank} {xp}', '{user.name}', 'Level {level} reached!', '   ', '{nothing} {level}', 'x'.repeat(5000), '{player.name} is level {level} with {xp} XP'];
  const payloads = [];
  for (const template of templates) {
    for (const useEmbed of [false, true]) {
      for (const dm of [false, true]) {
        for (const [rank, embedColor] of [[3, null], [null, 0x123456]]) {
          const p = svc.buildLevelUpPayload({ template, useEmbed, member: memberA, user: userA, guild: guildA, channel: channelA, level: 7, xp: 12345, rank, embedColor, dm });
          payloads.push({
            template, useEmbed, dm, rank, embedColor,
            out: p ? {
              content: p.content ?? null,
              embeds: p.embeds ? p.embeds.map(ej) : null,
              components: p.components ? p.components.map((c) => (typeof c.toJSON === 'function' ? c.toJSON() : c)) : null,
              allowedMentions: p.allowedMentions,
            } : null,
          });
        }
      }
    }
  }
  out.payloads = payloads;
  out.vars = svc.levelUpVars({ user: userA, member: memberA, level: 7, xp: 1234, rank: null });
  out.rankWhere = svc.rankWhere('g', 'u', 50);
  out.constants = {
    DEFAULT_LEVELUP_MESSAGE: svc.DEFAULT_LEVELUP_MESSAGE, ANNOUNCE_MODES: svc.ANNOUNCE_MODES, ANNOUNCE_MODE_LABELS: svc.ANNOUNCE_MODE_LABELS,
    VOICE_KEY_TTL_SECONDS: svc.VOICE_KEY_TTL_SECONDS, VOICE_CHECKPOINT_MS: svc.VOICE_CHECKPOINT_MS, ROLE_GIVE_MAX_MEMBERS: svc.ROLE_GIVE_MAX_MEMBERS,
    BULK_SYNC_MAX_MEMBERS: svc.BULK_SYNC_MAX_MEMBERS, LEADERBOARD_MAX_ROWS: svc.LEADERBOARD_MAX_ROWS, RAW_SQL: svc.RAW_SQL.map((r) => r.name),
  };

  // The cache keys, values and TTLs the JS writes (the cache is stubbed; nothing reaches Redis).
  const calls = [];
  cache.setNX = async (key, value, ttl) => { calls.push({ op: 'setNX', key, value, ttl }); return false; };
  cache.get = async (key) => { calls.push({ op: 'get', key }); return null; };
  cache.set = async (key, value, ttl) => { calls.push({ op: 'set', key, value, ttl }); return true; };
  cache.del = async (key) => { calls.push({ op: 'del', key }); return true; };
  const roles = { cache: new Map() };
  const settings = { noXpChannelIds: [], noXpRoleIds: [], xpCooldown: 0, xpMin: 15, xpMax: 25, xpMultiplier: 1, xpBoostRoles: null, voiceXpRate: 5, voiceXpEnabled: true };
  const guild = { id: '955009000000000001', afkChannelId: null, client: { readyTimestamp: 1 }, channels: { cache: new Map() } };
  const member = { id: '955009100000000001', user: { id: '955009100000000001', bot: false }, roles };
  await svc.handleMessageXp({ message: { channel: { id: '1' } }, member, guild, getSettings: async () => settings });
  await svc.handleMessageXp({ message: { channel: { id: '1' } }, member, guild, getSettings: async () => ({ ...settings, xpCooldown: 75.9 }) });
  const other = { id: '955009100000000002', user: { id: '955009100000000002', bot: false }, voice: {}, roles };
  const channel = { id: '955009300000000001', parentId: null, members: new Map([[other.id, other]]) };
  await svc.reconcileVoiceMember({ guild, member, state: { channel, selfDeaf: false, serverDeaf: false }, settings, now: 1_700_000_000_000 });
  out.cacheCalls = calls;
  return out;
}

// ── Database and Redis ────────────────────────────────────────────────────────

const levelsFields = (s) => ({
  xpEnabled: s.xpEnabled, xpMin: s.xpMin, xpMax: s.xpMax, xpCooldown: s.xpCooldown, xpMultiplier: s.xpMultiplier, voiceXpEnabled: s.voiceXpEnabled,
  voiceXpRate: s.voiceXpRate, levelUpMode: s.levelUpMode, levelUpChannelId: s.levelUpChannelId, levelUpMessage: s.levelUpMessage,
  levelUpUseEmbed: s.levelUpUseEmbed, noXpRoleIds: s.noXpRoleIds, noXpChannelIds: s.noXpChannelIds, xpBoostRoles: s.xpBoostRoles,
  rankCardStyle: s.rankCardStyle, levelRolesStack: s.levelRolesStack, levelRolesSyncOnDemotion: s.levelRolesSyncOnDemotion,
});

function voiceGuild(g, channelId, ids) {
  const roles = { cache: new Map() };
  const members = ids.map((id) => ({ id, user: { id, bot: false }, roles, voice: { selfDeaf: false, serverDeaf: false } }));
  const channel = { id: channelId, parentId: null, members: new Map(members.map((m) => [m.id, m])) };
  const states = members.map((m) => ({ channelId, channel, member: m, selfDeaf: false, serverDeaf: false }));
  for (const m of members) m.voice = states.find((s) => s.member === m);
  return {
    id: g, afkChannelId: null, client: { readyTimestamp: Date.now() - 3_600_000 }, channels: { cache: new Map([[channelId, channel]]) },
    voiceStates: { cache: new Map(states.map((s) => [s.member.id, s])) }, members: { cache: new Map(members.map((m) => [m.id, m])) },
  };
}

const [cmd, ...args] = process.argv.slice(2);
if (cmd !== 'golden') {
  // The cache answers "unavailable" until the connection is ready; wait for it (bounded) so reads and writes are real.
  await Promise.race([
    new Promise((r) => (redis.status === 'ready' ? r() : redis.once('ready', r))),
    new Promise((r) => setTimeout(r, 5000)),
  ]);
}
try {
  if (cmd === 'golden') {
    const dir = join(ROOT, 'rust', 'fixtures', 'golden', 'feat-levels');
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, 'pure.json'), `${JSON.stringify(await golden(), null, 1)}\n`);
    answer({ ok: true });
  } else if (cmd === 'apply') {
    const [g, u, delta, messages = '0', voiceMinutes = '0', touch = 'false'] = args;
    answer(await svc.applyXpDelta({ guildId: g, userId: u, delta: Number(delta), messages: Number(messages), voiceMinutes: Number(voiceMinutes), touchLastXp: touch === 'true' }));
  } else if (cmd === 'setxp') {
    answer(await svc.setXp({ guildId: args[0], userId: args[1], xp: Number(args[2]) }));
  } else if (cmd === 'give') {
    answer(await svc.giveXpToMembers({ guildId: args[0], userIds: JSON.parse(args[1]), amount: Number(args[2]) }));
  } else if (cmd === 'rank') {
    answer(await svc.getRank(args[0], args[1], Number(args[2])));
  } else if (cmd === 'load') {
    answer(await prisma.userLevel.findUnique({ where: { guildId_userId: { guildId: args[0], userId: args[1] } } }));
  } else if (cmd === 'rewards') {
    answer(await prisma.levelRole.findMany({ where: { guildId: args[0] }, orderBy: { level: 'asc' } }));
  } else if (cmd === 'addreward') {
    const [guildId, level, roleId] = args;
    await settingsSvc.ensureGuildSettings(guildId);
    answer(await prisma.levelRole.upsert({ where: { guildId_level: { guildId, level: Number(level) } }, update: { roleId }, create: { guildId, level: Number(level), roleId } }));
  } else if (cmd === 'settings') {
    answer(levelsFields(await settingsSvc.getSettings(args[0], { fresh: true })));
  } else if (cmd === 'boost') {
    const list = svc.normalizeBoostRoles(JSON.parse(args[1]));
    answer(levelsFields(await settingsSvc.updateSettingsWith(args[0], () => ({ xpBoostRoles: list }))));
  } else if (cmd === 'roledelete') {
    const [g, roleId] = args;
    await svc.handleRoleDelete({ guild: { id: g }, role: { id: roleId }, getSettings: () => settingsSvc.getSettingsSafe(g) });
    answer({ settings: levelsFields(await settingsSvc.getSettings(g, { fresh: true })), rewards: await prisma.levelRole.findMany({ where: { guildId: g } }) });
  } else if (cmd === 'seedvoice') {
    const [g, channelId, ...ids] = args;
    const guild = voiceGuild(g, channelId, ids);
    await svc.seedVoiceSessions({ guild, reason: 'startup', getSettings: () => settingsSvc.getSettingsSafe(g) });
    answer({ ok: true });
  } else if (cmd === 'voiceget') {
    const raw = await cache.get(`voice:start:${args[0]}:${args[1]}`);
    answer({ raw, parsed: svc.parseVoiceSession(raw) });
  } else if (cmd === 'job') {
    answer(await prisma.scheduledJob.findUnique({ where: { dedupeKey: args[0] } }));
  } else {
    throw new Error(`unknown command ${cmd}`);
  }
} catch (err) {
  answer({ error: String(err?.stack ?? err?.message ?? err) });
}
redis.disconnect();
await prisma.$disconnect().catch(() => {});
process.exit(0);
