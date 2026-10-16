// pkg-achievements tests (pkg-achievements.md §8 plus the architect amendments). Offline by default: the pure helpers
// (goals, tiers, rewards, progress maths, boost detection, announcement channel and text, badge/name/deadline parsing,
// member entries), the command payloads, the plugin declaration, the hook filters and a static scan. With
// TEST_DATABASE_URL (a migrated database) the DB cases also run: every RAW_SQL statement, the three recording phases
// (including concurrent first events), the compare-and-set claim, payout and announcements against fake Discord objects,
// the catch-up seeding, reset, tier changes and the /achievement-config and /achievements commands against fake
// interactions:
//   TEST_DATABASE_URL='<url>' node --test --test-concurrency=1 test/achievements.test.js
// NOTE: the database tests run on one Postgres connection (connection_limit=1), so concurrent writers are serialised
// by the pool but still interleave statement by statement.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';

process.env.NODE_ENV ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID ??= '000000000000000000';
// Nothing listens here: the cache fails open / closed exactly as it does in an outage.
process.env.REDIS_URL = process.env.TEST_REDIS_URL ?? 'redis://127.0.0.1:1';
process.env.DATABASE_URL = process.env.TEST_DATABASE_URL ?? process.env.DATABASE_URL ?? 'postgresql://test:test@127.0.0.1:1/test';

const DB = process.env.TEST_DATABASE_URL;
const skipDb = DB ? false : 'TEST_DATABASE_URL not set';

const { PermissionFlagsBits } = await import('discord.js');
const { Prisma } = await import('@prisma/client');
const svc = await import('../src/services/achievementService.js');
const cfg = await import('../src/commands/admin/achievement-config.js');
const mine = await import('../src/commands/community/achievements.js');
const { plugin } = await import('../src/plugins/achievements.js');
const { default: redis, cache } = await import('../src/services/redis.js');
const { default: prisma } = await import('../src/database/client.js');
const { collectPlugins } = await import('../src/handlers/pluginLoader.js');
const { buildCommandPayload } = await import('../src/handlers/commandLoader.js');
const { HOOK_ORDER } = await import('../src/config/hookPoints.js');
const { LIMITS, PERMISSION_LEVELS } = await import('../src/config/constants.js');
const { registerProvider } = await import('../src/services/rewards.js');
const { renderPlaceholders } = await import('../src/utils/placeholderParser.js');
const { ensureGuildSettings, updateSettings } = await import('../src/services/settingsService.js');

// Silence the Redis reconnect and logger noise (logger prints through console).
const quiet = { error: console.error, warn: console.warn, log: console.log, debug: console.debug, info: console.info };
console.error = console.warn = console.log = console.debug = console.info = () => {};

const G = (n) => `955027${String(n).padStart(12, '0')}`; // guild ids unique to this package (port 55027)
const ALL_GUILDS = Array.from({ length: 40 }, (_, i) => G(i + 1));
const realSetNX = cache.setNX;

after(async () => {
  cache.setNX = realSetNX;
  Object.assign(console, quiet);
  redis.disconnect();
  if (DB) {
    // The cascade removes Achievement, AchievementProgress and UserLevel rows.
    await prisma.guildSettings.deleteMany({ where: { guildId: { in: ALL_GUILDS } } }).catch(() => {});
  }
  await prisma.$disconnect().catch(() => {});
});

// ── Helpers ───────────────────────────────────────────────────────────────────

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, ms = 4000) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error('until: timed out');
    await sleep(15);
  }
}
function charCount(node) {
  let n = (node.name?.length ?? 0) + (node.description?.length ?? 0);
  for (const c of node.choices ?? []) n += c.name.length + String(c.value).length;
  for (const o of node.options ?? []) n += charCount(o);
  return n;
}
const rw = (type, v) => (type === 'ROLE_ADD' || type === 'ROLE_REMOVE' ? { type, roleId: v } : { type, amount: v });
const R1 = '123456789012345601';
const R2 = '123456789012345602';

// ── 1. parseGoals ─────────────────────────────────────────────────────────────

test('parseGoals accepts lists, suffixes and separators', () => {
  assert.deepEqual(svc.parseGoals('10,50,100', 'MESSAGES'), { ok: true, goals: [10, 50, 100] });
  assert.deepEqual(svc.parseGoals('1k, 5k > 10k', 'MESSAGES'), { ok: true, goals: [1000, 5000, 10000] });
  assert.deepEqual(svc.parseGoals('1.5k', 'MESSAGES'), { ok: true, goals: [1500] });
  assert.deepEqual(svc.parseGoals('1.1k', 'MESSAGES'), { ok: true, goals: [1100] }); // 1.1 * 1000 is 1100.0000000000002
  assert.deepEqual(svc.parseGoals('2m', 'MESSAGES'), { ok: true, goals: [2_000_000] });
  assert.deepEqual(svc.parseGoals('5;10|20/30', 'PURCHASES'), { ok: true, goals: [5, 10, 20, 30] });
});

test('parseGoals refuses bad input with the spec texts', () => {
  assert.match(svc.parseGoals('1.0005k', 'MESSAGES').error, /is not a whole number/);
  assert.match(svc.parseGoals('100 50', 'MESSAGES').error, /Goals must go up/);
  assert.match(svc.parseGoals('0', 'MESSAGES').error, /at least 1/);
  assert.match(svc.parseGoals('1 2 3 4 5', 'MESSAGES').error, /Give 1 to 4 goals/);
  assert.match(svc.parseGoals('1001', 'LEVEL').error, /Level reached goals can be at most 1,000/);
  assert.match(svc.parseGoals('101', 'BOOST').error, /Server boosts goals can be at most 100/);
  assert.match(svc.parseGoals('', 'MESSAGES').error, /Give 1 to 4 goals/);
  assert.match(svc.parseGoals('abc', 'MESSAGES').error, /"abc".*not a whole number/);
  assert.match(svc.parseGoals('10 10', 'MESSAGES').error, /Goals must go up/);
});

// ── 2/3. normalizeTiers, validateTiers ────────────────────────────────────────

test('normalizeTiers: invalid shapes give no tiers; names are relabelled; bad goals dropped', () => {
  for (const bad of [null, undefined, {}, 'x', 5, { version: 2, tiers: [] }]) assert.deepEqual(svc.normalizeTiers(bad), []);
  const relabelled = svc.normalizeTiers([{ name: 'Gold', goal: 10, rewards: [] }, { name: 'Bronze', goal: 20 }]);
  assert.deepEqual(relabelled.map((t) => t.name), ['Bronze', 'Silver']);
  assert.deepEqual(svc.normalizeTiers([{ goal: 0 }, { goal: 1.5 }, { goal: '5' }, { goal: 7 }]).map((t) => t.goal), [7]);
  assert.deepEqual(svc.normalizeTiers([{ goal: 10 }, { goal: 50 }, { goal: 40 }, { goal: 100 }]).map((t) => t.goal), [10, 50]);
  assert.equal(svc.normalizeTiers([1, 2, 3, 4, 5].map((goal) => ({ goal }))).length, 4);
  assert.deepEqual(svc.normalizeTiers([null, 'x', { goal: 3 }]).map((t) => t.goal), [3]);
  assert.deepEqual(svc.normalizeTiers([{ goal: 10_000_001 }]), []);
});

test('normalizeTiers: rewards are validated, de-duplicated and ordered', () => {
  const [t] = svc.normalizeTiers([{
    goal: 5,
    rewards: [
      rw('COINS', 5), rw('XP', 10), rw('XP', 99), rw('ROLE_REMOVE', R1), rw('ROLE_ADD', R1), rw('ROLE_ADD', R2),
      { type: 'NOPE', amount: 1 }, rw('ROLE_ADD', 'abc'), rw('COINS', 0), rw('XP', 1_000_001), { type: 'XP', amount: 1.5, extra: 1 },
    ],
  }]);
  // ROLE_REMOVE == ROLE_ADD role is dropped; the duplicate XP/ROLE_ADD are dropped; order is fixed
  assert.deepEqual(t.rewards, [rw('ROLE_ADD', R1), rw('XP', 10), rw('COINS', 5)]);
  assert.deepEqual(svc.normalizeRewards([rw('ROLE_REMOVE', R2), { type: 'XP', amount: 5, junk: true }]), [rw('ROLE_REMOVE', R2), rw('XP', 5)]);
  assert.deepEqual(svc.normalizeRewards('x'), []);
  assert.deepEqual(svc.normalizeRewards([rw('XP', 0), rw('XP', 7)]), [rw('XP', 7)]); // the first VALID one of a type
});

test('validateTiers and serializeTiers', () => {
  assert.equal(svc.validateTiers([], 'MESSAGES').ok, false);
  assert.equal(svc.validateTiers([{ goal: 1001 }], 'LEVEL').ok, false);
  assert.equal(svc.validateTiers([1, 2, 3, 4, 5].map((goal) => ({ goal })), 'MESSAGES').ok, false);
  assert.equal(svc.validateTiers([{ goal: 5 }, { goal: 5 }], 'MESSAGES').ok, false);
  assert.equal(svc.validateTiers([{ goal: 1, rewards: [rw('XP', 0)] }], 'MESSAGES').ok, false);
  const four = [10, 20, 30, 40].map((goal) => ({ goal, rewards: [rw('XP', 5)] }));
  assert.equal(svc.validateTiers(four, 'MESSAGES').ok, true);
  assert.equal(svc.validateTiers([{ goal: 1 }], 'LEVEL').ok, true); // rewards may be absent
  const out = svc.serializeTiers([{ goal: 3, rewards: [rw('XP', 1)] }]);
  assert.deepEqual(out, [{ name: 'Bronze', goal: 3, rewards: [rw('XP', 1)] }]);
});

// ── 4. evaluateProgress ───────────────────────────────────────────────────────

test('evaluateProgress', () => {
  const tiers = [10, 50, 100].map((goal) => ({ goal, rewards: [] }));
  const row = (progress, tier = 0, almostNotified = false) => ({ progress, tier, almostNotified });
  assert.deepEqual(svc.evaluateProgress(tiers, row(9)), { advanceTo: null, almost: { tierIndex: 0, goal: 10 } });
  assert.deepEqual(svc.evaluateProgress(tiers, row(9, 0, true)), { advanceTo: null, almost: null });
  assert.equal(svc.evaluateProgress(tiers, row(10)).advanceTo, 1);
  assert.equal(svc.evaluateProgress(tiers, row(120)).advanceTo, 3); // multi-tier jump
  assert.deepEqual(svc.evaluateProgress(tiers, row(45, 1)).almost, { tierIndex: 1, goal: 50 });
  assert.equal(svc.evaluateProgress(tiers, row(44, 1)).almost, null);
  assert.deepEqual(svc.evaluateProgress(tiers, row(120, 3)), { advanceTo: null, almost: null });
  assert.deepEqual(svc.evaluateProgress(tiers, row(120, 5)), { advanceTo: null, almost: null }); // stale tier > length
  assert.deepEqual(svc.evaluateProgress([{ goal: 9, rewards: [] }], row(8)), { advanceTo: null, almost: null });
  assert.deepEqual(svc.evaluateProgress(tiers, row(0)), { advanceTo: null, almost: null });
  assert.equal(svc.evaluateProgress(tiers, row(10, 1)).advanceTo, null); // already claimed
});

// ── 5. isBoostStart ───────────────────────────────────────────────────────────

test('isBoostStart', () => {
  const now = 1_700_000_000_000;
  assert.equal(svc.isBoostStart({ oldKnown: true, oldMember: { premiumSinceTimestamp: null }, newMember: { premiumSinceTimestamp: now } }, now), true);
  assert.equal(svc.isBoostStart({ oldKnown: true, oldMember: { premiumSinceTimestamp: now - 9000 }, newMember: { premiumSinceTimestamp: now } }, now), false);
  assert.equal(svc.isBoostStart({ oldKnown: false, oldMember: {}, newMember: { premiumSinceTimestamp: now - 30_000 } }, now), true);
  assert.equal(svc.isBoostStart({ oldKnown: false, oldMember: {}, newMember: { premiumSinceTimestamp: now - 5 * 60_000 } }, now), false);
  assert.equal(svc.isBoostStart({ oldKnown: true, oldMember: { premiumSinceTimestamp: 5 }, newMember: { premiumSinceTimestamp: null } }, now), false);
  assert.equal(svc.isBoostStart({ oldKnown: false, newMember: { premiumSinceTimestamp: now + 60_000 } }, now), false); // from the future
  assert.equal(svc.isBoostStart({}, now), false);
});

// ── 6. pickAnnounceChannel ────────────────────────────────────────────────────

test('pickAnnounceChannel', () => {
  const A = { id: 'A', usable: true };
  const B = { id: 'B', usable: false };
  const ok = (ch) => ch.usable;
  assert.equal(svc.pickAnnounceChannel('CURRENT', { source: A, configured: B }, ok), A);
  assert.equal(svc.pickAnnounceChannel('CURRENT', { source: null, configured: A }, ok), A);
  assert.equal(svc.pickAnnounceChannel('CURRENT', { source: B, configured: null }, ok), null);
  assert.equal(svc.pickAnnounceChannel('CURRENT', { source: B, configured: A }, ok), A);
  assert.equal(svc.pickAnnounceChannel('CHANNEL', { source: A, configured: B }, ok), A);
  assert.equal(svc.pickAnnounceChannel('CHANNEL', { source: B, configured: A }, ok), A);
  assert.equal(svc.pickAnnounceChannel('OFF', { source: A, configured: A }, ok), null);
  assert.equal(svc.pickAnnounceChannel('DM', { source: A, configured: A }, ok), null);
  assert.equal(svc.pickAnnounceChannel('CURRENT', {}, ok), null);
});

// ── 7. Announcement text ──────────────────────────────────────────────────────

test('announcement variables render through the default message', () => {
  const tiers = svc.normalizeTiers([{ goal: 10 }, { goal: 50 }, { goal: 100 }]);
  const a = { name: 'Chatterbox', description: 'Talk a lot', tiers };
  const member = { id: '1', displayName: 'Ann', user: { id: '1', username: 'ann' } };
  const render = (template, vars) => renderPlaceholders(template, { member, user: member.user, guild: { id: 'g', name: 'Guild' }, vars });
  const vars = svc.buildAnnouncementVars({ achievement: a, tierIndex: 2, progress: 1234, userId: '1', displayName: 'Ann*', rewardsText: '+100 XP' });
  assert.equal(render(svc.DEFAULT_ACHIEVEMENT_MESSAGE, vars), '🏆 <@1> unlocked **Chatterbox (Gold)**!');
  assert.equal(render('{tier} {tier.emoji} {goal} {progress} {rewards} {achievement.name} {achievement.description}', vars), 'Gold 🥇 100 1,234 +100 XP Chatterbox Talk a lot');
  assert.equal(render('{player.name}', vars), 'Ann\\*');
  const single = svc.buildAnnouncementVars({ achievement: { name: 'Booster', tiers: svc.normalizeTiers([{ goal: 1 }]) }, tierIndex: 0, progress: 1, userId: '1', displayName: 'Ann' });
  assert.equal(render(svc.DEFAULT_ACHIEVEMENT_MESSAGE, single), '🏆 <@1> unlocked **Booster**!');
  assert.equal(single.tier, 'Bronze');
  assert.equal(single.rewards, '');
  assert.equal(svc.achievementLabel(a, 0), 'Chatterbox (Bronze)');
  const sample = svc.sampleVars({ id: '9', displayName: 'Bo' });
  assert.equal(sample.player, '<@9>');
  assert.equal(sample.achievement, 'Chatterbox (Gold)');
  assert.equal(sample.goal, '2,500');
});

// ── 8. Badge emoji ────────────────────────────────────────────────────────────

test('normalizeBadgeEmoji', () => {
  for (const good of ['🏆', '🇺🇸', '👨‍👩‍👧', '❤️']) assert.equal(svc.normalizeBadgeEmoji(good).ok, true, good);
  for (const bad of ['abc', '12', '🏆 🏆', '', '🏆'.repeat(20)]) assert.equal(svc.normalizeBadgeEmoji(bad).ok, false, bad);
  const custom = '<:x:123456789012345678>';
  assert.equal(svc.normalizeBadgeEmoji(custom, { hasCustomEmoji: () => true }).ok, true);
  assert.match(svc.normalizeBadgeEmoji(custom, { hasCustomEmoji: () => false }).error, /custom emojis from servers I'm in/);
  assert.deepEqual(svc.normalizeBadgeEmoji('none', { allowClear: true }), { ok: true, value: null });
  assert.equal(svc.normalizeBadgeEmoji('NONE', { allowClear: true }).value, null);
  assert.equal(svc.normalizeBadgeEmoji('none').ok, false);
});

// ── 9. Names ──────────────────────────────────────────────────────────────────

test('normalizeAchievementName', () => {
  assert.equal(svc.normalizeAchievementName('  Chatter\n box '), 'Chatter box');
  assert.equal(svc.normalizeAchievementName(''), null);
  assert.equal(svc.normalizeAchievementName('   '), null);
  assert.equal(svc.normalizeAchievementName('x'.repeat(51)), null);
  assert.equal(svc.normalizeAchievementName('x'.repeat(50)), 'x'.repeat(50));
  assert.equal(svc.normalizeAchievementName(null), null);
});

// ── 10. Deadlines ─────────────────────────────────────────────────────────────

test('parseDeadline', () => {
  const now = new Date('2026-06-01T12:00:00Z');
  const d = svc.parseDeadline('30d', 'UTC', { now });
  assert.equal(d.ok, true);
  assert.equal(d.value.getTime(), now.getTime() + 30 * 86_400_000);
  assert.deepEqual(svc.parseDeadline('none', 'UTC', { now, allowClear: true }), { ok: true, value: null });
  assert.deepEqual(svc.parseDeadline('OFF', 'UTC', { now, allowClear: true }), { ok: true, value: null });
  assert.match(svc.parseDeadline('none', 'UTC', { now }).error, /Use a date like 2026-12-31 23:59/);
  assert.match(svc.parseDeadline('2000-01-01', 'UTC', { now }).error, /in the future/);
  assert.match(svc.parseDeadline('30s', 'UTC', { now }).error, /in the future/);
  assert.match(svc.parseDeadline('6y', 'UTC', { now }).error, /at most 5 years/);
  assert.match(svc.parseDeadline('2040-01-01', 'UTC', { now }).error, /at most 5 years/);
  assert.equal(svc.parseDeadline('2026-12-31 23:59', 'UTC', { now }).value.toISOString(), '2026-12-31T23:59:00.000Z');
  assert.match(svc.parseDeadline('garbage', 'Europe/Paris', { now }).error, /server time zone: Europe\/Paris/);
});

// ── 11/12. Rewards merge, tier removal map ────────────────────────────────────

test('mergeRewards and remapTierAfterRemoval', () => {
  assert.deepEqual(svc.mergeRewards([rw('XP', 10), rw('ROLE_ADD', R1)], [rw('XP', 20)], new Set(['XP'])), [rw('ROLE_ADD', R1), rw('XP', 20)]);
  assert.deepEqual(svc.mergeRewards([], [rw('XP', 20)], new Set(['XP'])), [rw('XP', 20)]);
  assert.deepEqual(svc.mergeRewards([rw('XP', 10)], [], new Set()), [rw('XP', 10)]);
  assert.deepEqual(svc.mergeRewards([rw('ROLE_ADD', R1)], [rw('ROLE_REMOVE', R1)], new Set(['ROLE_REMOVE'])), [rw('ROLE_ADD', R1)]);
  assert.equal(svc.remapTierAfterRemoval(2, 1), 1);
  assert.equal(svc.remapTierAfterRemoval(3, 1), 2);
  assert.equal(svc.remapTierAfterRemoval(1, 1), 1);
  assert.equal(svc.remapTierAfterRemoval(0, 0), 0);
});

// ── 13. displayProgress ───────────────────────────────────────────────────────

test('displayProgress', () => {
  const tiers = svc.normalizeTiers([{ goal: 100 }]);
  const base = { trigger: 'MESSAGES', tiers, trackPastProgress: true };
  assert.deepEqual(svc.displayProgress(base, null, { messageCount: 120, level: 3 }, null), { progress: 120, tier: 0, pending: true });
  assert.deepEqual(svc.displayProgress(base, null, { messageCount: 50 }, null), { progress: 50, tier: 0, pending: false });
  assert.deepEqual(svc.displayProgress({ ...base, trackPastProgress: false }, null, { messageCount: 120 }, null), { progress: 0, tier: 0, pending: false });
  assert.deepEqual(svc.displayProgress(base, { progress: 7, tier: 0 }, { messageCount: 120 }, null), { progress: 7, tier: 0, pending: false });
  assert.deepEqual(svc.displayProgress(base, { progress: 100, tier: 9 }, null, null), { progress: 100, tier: 1, pending: false });
  assert.deepEqual(svc.displayProgress({ ...base, trigger: 'LEVEL' }, null, null, null), { progress: 0, tier: 0, pending: false });
  assert.equal(svc.displayProgress({ ...base, trigger: 'LEVEL' }, null, { level: 12 }, null).progress, 12);
  assert.equal(svc.displayProgress({ ...base, trigger: 'BOOST', tiers: svc.normalizeTiers([{ goal: 1 }]) }, null, null, { premiumSinceTimestamp: 5 }).pending, true);
  assert.equal(svc.displayProgress({ ...base, trigger: 'REACTIONS' }, null, { messageCount: 500 }, null).progress, 0);
});

// ── 14. formatMemberEntry ─────────────────────────────────────────────────────

test('formatMemberEntry', () => {
  const lite = (over = {}) => ({
    id: 'a', name: 'Chatterbox', description: null, emoji: null, trigger: 'MESSAGES', trackPastProgress: true, deadlineMs: null,
    tiers: svc.normalizeTiers([{ goal: 100 }, { goal: 500 }, { goal: 2500 }, { goal: 10000 }]), ...over,
  });
  const now = Date.now();
  const done = svc.formatMemberEntry(lite(), { progress: 12345, tier: 4, pending: false }, now);
  assert.match(done, /✅ Completed · 12,345 messages/);
  assert.match(done, /💬 \*\*Chatterbox\*\* — 💎 Diamond/);
  const mid = svc.formatMemberEntry(lite({ tiers: lite().tiers.slice(0, 3) }), { progress: 130, tier: 1 }, now);
  assert.match(mid, /🥈 Silver/);
  assert.match(mid, /130\/500 → 🥈 Silver/);
  const single = svc.formatMemberEntry(lite({ tiers: lite().tiers.slice(0, 1) }), { progress: 100, tier: 1 }, now);
  assert.match(single, /— ✅ Unlocked/);
  assert.match(single, /✅ Completed · 100 messages/);
  assert.match(svc.formatMemberEntry(lite({ trigger: 'LEVEL' }), { progress: 7, tier: 0 }, now), /▰.*7\/100/);
  assert.match(svc.formatMemberEntry(lite({ trigger: 'LEVEL', tiers: lite().tiers.slice(0, 1) }), { progress: 120, tier: 1 }, now), /Completed · level 120/);
  assert.match(svc.formatMemberEntry(lite({ deadlineMs: now - 1000 }), { progress: 1, tier: 0 }, now), /⌛ ended/);
  assert.match(svc.formatMemberEntry(lite({ deadlineMs: now + 86_400_000 }), { progress: 1, tier: 0 }, now), /⏳ ends <t:\d+:R>/);
  assert.match(svc.formatMemberEntry(lite(), { progress: 1, tier: 0, enabled: false }, now), /\(retired\)/);
  assert.match(svc.formatMemberEntry(lite(), { progress: 150, tier: 0, pending: true }, now), /unlocks on your next activity/);
  const long = svc.formatMemberEntry(lite({ name: 'n'.repeat(50), description: 'd'.repeat(200), emoji: '<:abcdefghij:123456789012345678>' }), { progress: 1, tier: 1 }, now);
  for (const line of long.split('\n')) assert.ok(line.length <= 300, `line of ${line.length}`);
  assert.equal(long.split('\n').length, 3);
});

// ── 15. Reward availability (registers the fake providers the later tests use) ─

const xpGrants = [];
const coinGrants = [];
const xpProviderResult = { value: null }; // a test can make the xp provider refuse
test('rewardUnavailableMessage and formatRewards', () => {
  // No provider registered yet in this process.
  assert.match(svc.rewardUnavailableMessage('xp', { xpEnabled: true }), /XP rewards are not available on this bot \(the ⭐ Levels module is not installed\)/);
  assert.match(svc.rewardUnavailableMessage('coins', { economyEnabled: true }), /Coin rewards are not available on this bot \(the 🪙 Economy module is not installed\)/);
  registerProvider('xp', { grant: async (a) => { xpGrants.push(a); return xpProviderResult.value ?? { ok: true, total: a.amount }; }, take: async () => ({ ok: true }) }, 'test');
  registerProvider('coins', { grant: async (a) => { coinGrants.push(a); return { ok: true, total: a.amount }; }, take: async () => ({ ok: true }) }, 'test');
  assert.match(svc.rewardUnavailableMessage('xp', { xpEnabled: false }), /XP rewards need the \*\*⭐ Levels\*\* module\. Enable it with `\/module enable leveling`/);
  assert.match(svc.rewardUnavailableMessage('coins', { economyEnabled: false }), /\/module enable economy/);
  assert.equal(svc.rewardUnavailableMessage('xp', { xpEnabled: true }), null);
  assert.equal(svc.rewardUnavailableMessage('coins', { economyEnabled: true }), null);

  const guild = { roles: { cache: new Map([[R1, {}]]) } };
  const text = svc.formatRewards([rw('ROLE_ADD', R1), rw('ROLE_REMOVE', R2), rw('XP', 1500), rw('COINS', 20)], guild, { currencyName: 'gold', currencyEmoji: '🪙' });
  assert.equal(text, `<@&${R1}>, −@deleted-role ⚠️, +1,500 XP, +20 🪙 gold`);
  assert.equal(svc.formatRewards([rw('COINS', 1)], null, null), '+1 🪙 coins');
  assert.equal(svc.formatRewards([], null, null), '');
  assert.ok(svc.formatRewards(Array.from({ length: 100 }, () => rw('XP', 123456)), null, null).length <= 900);
});

// ── 16. Command payloads ──────────────────────────────────────────────────────

test('command payloads', () => {
  const a = buildCommandPayload(mine);
  const c = buildCommandPayload(cfg);
  assert.ok(charCount(a) <= 260, `achievements ${charCount(a)}`);
  assert.ok(charCount(c) <= 2800, `achievement-config ${charCount(c)}`);
  assert.deepEqual(c.options.map((o) => o.name), ['create', 'edit', 'delete', 'enable', 'disable', 'list', 'info', 'reset', 'preset', 'tier', 'announce']);
  const group = (name) => c.options.find((o) => o.name === name);
  assert.deepEqual(group('tier').options.map((o) => o.name), ['add', 'edit', 'remove']);
  assert.deepEqual(group('announce').options.map((o) => o.name), ['mode', 'channel', 'message']);
  const create = group('create');
  assert.equal(create.options.length, 12); // + new_role_add (newTargets)
  assert.deepEqual(create.options.slice(0, 3).map((o) => [o.name, o.required]), [['name', true], ['trigger', true], ['goals', true]]);
  assert.equal(create.options.find((o) => o.name === 'trigger').choices.length, 5);
  assert.deepEqual(group('announce').options[0].options.map((o) => o.name), ['mode', 'channel', 'new_channel', 'embed']);
  const walk = (node, fn) => { fn(node); for (const o of node.options ?? []) walk(o, fn); };
  const achOptions = [];
  walk(c, (n) => { if (n.name === 'achievement' && n.type === 3) achOptions.push(n); });
  assert.equal(achOptions.length, 9); // edit, delete, enable, disable, info, reset, tier add|edit|remove
  for (const o of achOptions) assert.equal(o.autocomplete, true);
  walk(c, (n) => { assert.ok((n.options?.length ?? 0) <= 12); assert.ok((n.description?.length ?? 0) <= 100); });
  assert.deepEqual(a.contexts, [0]);
  assert.deepEqual(c.contexts, [0]);
  assert.equal(mine.module, 'achievements');
  assert.equal(cfg.module, 'achievements');
  assert.equal(mine.gated, true);
  assert.equal(cfg.gated, false);
  assert.equal(mine.permissionLevel, PERMISSION_LEVELS.MEMBER);
  assert.equal(cfg.permissionLevel, PERMISSION_LEVELS.ADMIN);
  assert.equal(c.default_member_permissions, String(PermissionFlagsBits.ManageGuild));
  assert.equal(a.default_member_permissions, undefined);
  assert.equal(typeof cfg.autocomplete, 'function');
  assert.equal(mine.autocomplete, undefined);
  assert.equal(c.options.find((o) => o.name === 'reset').options.find((o) => o.name === 'achievement').required ?? false, false);
});

// ── 17. Plugin ────────────────────────────────────────────────────────────────

test('plugin declaration', async () => {
  const { plugins, errors } = await collectPlugins();
  assert.deepEqual(errors.filter((e) => /achievements/.test(e)), []);
  const p = plugins.find((x) => x.id === 'achievements');
  assert.ok(p);
  assert.equal(p.module, 'achievements');
  assert.equal(p.hooks.length, 5);
  assert.deepEqual(p.hooks.map((h) => h.event).sort(), ['economyPurchase', 'guildMemberUpdate', 'levelUp', 'messageCreate', 'messageReactionAdd']);
  for (const h of p.hooks) {
    assert.equal(h.id, 'achievements');
    assert.equal(typeof h.filter, 'function');
    assert.equal(typeof h.run, 'function');
    assert.ok(HOOK_ORDER[h.event].includes('achievements'));
  }
  assert.deepEqual(p.components, []);
  assert.deepEqual(p.jobs, []);
  assert.deepEqual(plugin.provides, {});
  assert.equal(plugin.hooks.find((h) => h.event === 'messageCreate').timeoutMs, 5000);
});

test('hook filters', () => {
  svc.__resetAchievementState();
  assert.equal(svc.messageFilter({ guildId: 'g', message: { author: { bot: true } } }), false);
  assert.equal(svc.messageFilter({ guildId: null, message: { author: { bot: false } } }), false);
  assert.equal(svc.messageFilter({ guildId: 'g', message: { author: { bot: false } } }), true); // unknown index: let the hook load it
  assert.equal(svc.reactionFilter({ guildId: 'g', user: { bot: true } }), false);
  assert.equal(svc.reactionFilter({ guildId: 'g', user: { bot: false } }), true);
  const t = Date.now();
  assert.equal(svc.memberUpdateFilter({ guildId: 'g', newMember: { user: { bot: true }, premiumSinceTimestamp: t }, oldKnown: true, oldMember: {} }), false);
  assert.equal(svc.memberUpdateFilter({ guildId: 'g', newMember: { user: { bot: false }, premiumSinceTimestamp: null }, oldKnown: true, oldMember: {} }), false);
  assert.equal(svc.memberUpdateFilter({ guildId: 'g', newMember: { user: { bot: false }, premiumSinceTimestamp: t }, oldKnown: true, oldMember: { premiumSinceTimestamp: t } }), false);
  assert.equal(svc.memberUpdateFilter({ guildId: 'g', newMember: { user: { bot: false }, premiumSinceTimestamp: t }, oldKnown: true, oldMember: {} }), true);
  assert.equal(svc.levelUpFilter({ guildId: null }), false);
  assert.equal(svc.purchaseFilter({ guildId: 'g' }), true);
});

test('activeAchievements, buildIndex, toLite and mayHaveTriggers', () => {
  const now = 1000;
  const a = (over) => ({ id: 'x', trigger: 'MESSAGES', tiers: [{ goal: 1 }], deadlineMs: null, ...over });
  assert.equal(svc.activeAchievements([a({}), a({ tiers: [] }), a({ deadlineMs: 999 }), a({ deadlineMs: 1000 })], now).length, 2);
  const idx = svc.buildIndex([a({ trigger: 'LEVEL' }), a({})]);
  assert.deepEqual(Object.keys(idx.byTrigger), [...svc.TRIGGERS]);
  assert.equal(idx.byTrigger.LEVEL.length, 1);
  assert.equal(idx.byTrigger.BOOST.length, 0);
  const lite = svc.toLite({ id: 'i', name: 'N', description: null, emoji: null, trigger: 'BOOST', tiers: [{ name: 'X', goal: 2, rewards: [] }], trackPastProgress: false, deadline: new Date(5000) });
  assert.deepEqual(lite, { id: 'i', name: 'N', description: null, emoji: null, trigger: 'BOOST', tiers: [{ name: 'Bronze', goal: 2, rewards: [] }], trackPastProgress: false, deadlineMs: 5000 });
  svc.__resetAchievementState();
  assert.equal(svc.mayHaveTriggers('unknown-guild', ['LEVEL']), true);
});

test('RAW_SQL builders use SQL_NOW and explicit casts', () => {
  assert.equal(svc.RAW_SQL.length, 2);
  const names = new Set();
  for (const { name, build } of svc.RAW_SQL) {
    assert.ok(!names.has(name));
    names.add(name);
    const q = build();
    assert.equal(typeof q.sql, 'string', name);
    assert.ok(Array.isArray(q.values), name);
    assert.doesNotMatch(q.sql, /(?<![\w.])now\(\)(?!\s*AT\s+TIME\s+ZONE\s+'UTC')/i, name);
    assert.match(q.sql, /"updatedAt"/);
  }
  const insert = svc.RAW_SQL.find((r) => r.name === 'achievements.progressInsert').build().sql;
  assert.match(insert, /"id"/);
  assert.match(insert, /ON CONFLICT \("achievementId", "userId"\) DO NOTHING/);
  assert.match(insert, /unnest\(\?::text\[\], \?::text\[\], \?::int\[\]\)/);
});

// ── 18. Static scan ───────────────────────────────────────────────────────────

test('static scan of the four owned source files', () => {
  const files = ['src/services/achievementService.js', 'src/plugins/achievements.js', 'src/commands/community/achievements.js', 'src/commands/admin/achievement-config.js'];
  const banned = [
    /setCustomId\(/, /customId:/, /console\./, /setInterval/, /roles\.(add|set|remove)\(\[/,
    /from '\.\.\/(\.\.\/)?services\/(xpService|economyService)/, /parse: \[[^\]]*everyone/, /guild:\$\{[^}]*\}:settings/,
    /achievementProgress\.(delete|deleteMany)\(/, /LIMITS\.[A-Z_]+\s*\?\?/, /replace\('\{/,
  ];
  for (const f of files) {
    const text = readFileSync(new URL(`../${f}`, import.meta.url), 'utf8');
    for (const re of banned) assert.doesNotMatch(text, re, `${f} matches ${re}`);
    // The only timer is the message buffer's flush (one per waiting member, started by a message, never at import).
    assert.equal([...text.matchAll(/setTimeout\(/g)].length, f === 'src/services/achievementService.js' ? 1 : 0, `${f}: setTimeout`);
    for (const m of text.matchAll(/(?:\$queryRaw|\$executeRaw|Prisma\.sql)\s*`([\s\S]*?)`/g)) {
      assert.doesNotMatch(m[1], /(?<![\w.])now\(\)(?!\s*AT\s+TIME\s+ZONE\s+'UTC')/i, `${f} has a bare now()`);
    }
  }
  const service = readFileSync(new URL('../src/services/achievementService.js', import.meta.url), 'utf8');
  assert.doesNotMatch(service, /configLocks\.run\([^)]*\)\s*;?\s*\/\/ progress/); // the lock is never on the progress path
  assert.equal([...service.matchAll(/configLocks\.run/g)].length, 1); // only withConfigLock
});

test('offline: the index fails closed and recording never throws when Postgres is unreachable', { skip: DB ? 'offline only' : false }, async () => {
  svc.__resetAchievementState();
  const gid = '955027000000099999';
  const ctx = { guildId: gid, guild: {}, getSettings: async () => ({}) };
  const lite = { id: 'a', name: 'N', trigger: 'MESSAGES', tiers: svc.normalizeTiers([{ goal: 3 }]), trackPastProgress: true, deadlineMs: null };
  const [index, recorded, hooked, found, view] = await Promise.all([
    svc.getAchievementIndex(gid),
    svc.recordProgress({ ctx, settings: {}, userId: '1', trigger: 'MESSAGES', achievements: [lite], inc: 1 }),
    svc.onMessageCreate({ ...ctx, message: { author: { id: '1' } } }),
    svc.searchAchievements(gid, 'x'),
    svc.loadMemberView(gid, '1', null),
  ]);
  assert.equal(index, null);
  assert.equal(recorded, null);
  assert.equal(hooked, undefined);
  assert.deepEqual(found, []);
  assert.equal(view, null);
});

// ══ Database cases ═══════════════════════════════════════════════════════════════════════════════════════════════

// ── Fake Discord objects ──────────────────────────────────────────────────────

function makeGuild(id) {
  const channels = new Map();
  const roles = new Map();
  const members = new Map();
  const me = { id: 'bot', permissions: { has: () => true }, roles: { highest: { position: 100 } } };
  const guild = {
    id, name: 'Test *Guild*', ownerId: 'owner', available: true,
    channels: { cache: channels }, roles: { cache: roles },
    members: {
      me, cache: members,
      fetch: async (uid) => {
        const m = members.get(uid);
        if (!m) throw Object.assign(new Error('Unknown Member'), { code: 10007 });
        return m;
      },
    },
  };
  guild.client = { users: { fetch: async (uid) => members.get(uid)?.user ?? null }, emojis: { cache: new Map() } };
  return guild;
}
function addChannel(guild, id, { usable = true, failWith = null } = {}) {
  const sent = [];
  const ch = {
    id, name: `chan-${id}`, guild, sent,
    isTextBased: () => true, isThread: () => false,
    permissionsFor: () => ({ has: () => usable }),
    send: async (payload) => {
      if (failWith) throw Object.assign(new Error('send failed'), { code: failWith });
      sent.push(payload);
      return { id: `m${sent.length}` };
    },
    toString: () => `<#${id}>`,
  };
  guild.channels.cache.set(id, ch);
  return ch;
}
function addRole(guild, id, { position = 1, managed = false } = {}) {
  const role = { id, position, managed, permissions: { has: () => false }, toString: () => `<@&${id}>` };
  guild.roles.cache.set(id, role);
  return role;
}
function addMember(guild, id, { premium = null, roles = [] } = {}) {
  const roleCache = new Map(roles.map((r) => [r, true]));
  const dms = [];
  const ops = [];
  const send = async (p) => { dms.push(p); return { id: 'dm' }; };
  const m = {
    id, guild, dms, ops, displayName: `User ${id}`, premiumSinceTimestamp: premium,
    user: { id, bot: false, username: `user${id}`, globalName: null, send, toString: () => `<@${id}>` },
    roles: {
      cache: roleCache, highest: { position: 1 },
      add: async (rid) => { ops.push(['add', rid]); roleCache.set(rid, true); },
      remove: async (rid) => { ops.push(['remove', rid]); roleCache.delete(rid); },
    },
    send,
    displayAvatarURL: () => 'https://cdn.example/avatar.png',
    toString: () => `<@${id}>`,
  };
  guild.members.cache.set(id, m);
  return m;
}
const SETTINGS = (over = {}) => ({
  achievementsEnabled: true, achievementAnnounceMode: 'CURRENT', achievementChannelId: null, achievementMessage: null,
  achievementUseEmbed: false, currencyName: 'coins', currencyEmoji: '🪙', embedColor: null, timezone: 'UTC',
  xpEnabled: true, economyEnabled: true, ...over,
});
const ctxOf = (guild, settings, extra = {}) => ({
  guildId: guild.id, guild, client: guild.client, getSettings: async () => settings, ...extra,
});
const resetGrants = () => { xpGrants.length = 0; coinGrants.length = 0; };

/** Creates a guild row, an achievement and refreshes the index. */
async function seedAchievement(gid, { name = 'Chatterbox', trigger = 'MESSAGES', goals = [3], rewards = [], tiers = null, trackPastProgress = true, deadline = null, emoji = null, description = null } = {}) {
  await ensureGuildSettings(gid);
  const r = await svc.createAchievement(gid, {
    name, trigger, trackPastProgress, deadline, emoji, description, tiers: tiers ?? goals.map((goal) => ({ goal, rewards })),
  });
  assert.equal(r.ok, true, JSON.stringify(r));
  await svc.refreshAchievementCache(gid);
  return r.achievement;
}
const rowOf = (achievementId, userId) => prisma.achievementProgress.findUnique({ where: { achievementId_userId: { achievementId, userId } } });
const msgCtx = (guild, settings, member, channel) => ctxOf(guild, settings, { message: { author: { id: member.id, bot: false } }, member, channel });
// The DB cases check each message's effect right away: write every message at once (the buffer has its own offline test).
svc.__setMessageFlush({ delayMs: 0 });

// ── RAW_SQL ───────────────────────────────────────────────────────────────────

test('DB: every RAW_SQL statement executes against the migrated schema (as a generic runner would run it)', { skip: skipDb }, async () => {
  for (const { name, build } of svc.RAW_SQL) {
    await prisma.$queryRaw(Prisma.sql`EXPLAIN ${build()}`).catch((err) => assert.fail(`EXPLAIN ${name}: ${err.message}`));
    assert.deepEqual(await prisma.$queryRaw(build()).catch((err) => assert.fail(`${name}: ${err.message}`)), [], name);
  }
});

// ── Recording: phases, claim, payout, announcement ────────────────────────────

test('offline: the message buffer writes one +n UPDATE per member, flushes at the cap, on a changed set and on demand', { skip: DB ? 'offline only' : false }, async () => {
  svc.__resetAchievementState();
  svc.__setMessageFlush({ delayMs: 60, at: 5 });
  const gid = '955027000000088888';
  const ach = (id, deadline = null) => ({
    id, guildId: gid, name: id, trigger: 'MESSAGES', enabled: true, trackPastProgress: false, deadline,
    tiers: [{ goal: 1000, rewards: [] }], createdAt: new Date(0),
  });
  let rows = [ach('a1')];
  const updates = [];
  const real = { findMany: prisma.achievement.findMany, queryRaw: prisma.$queryRaw, upsert: prisma.guildSettings.upsert, tx: prisma.$transaction };
  prisma.achievement.findMany = async () => rows;
  prisma.guildSettings.upsert = async () => ({});
  prisma.$transaction = async () => { updates.push({ reset: true }); return []; };
  // Every progress UPDATE "finds" its row, so no INSERT (Phase 2) runs; the goal is far away, so nothing unlocks.
  prisma.$queryRaw = async (sql) => {
    const inc = sql.values[0];
    const ids = sql.values.find((v) => Array.isArray(v));
    updates.push({ user: sql.values.find((v) => typeof v === 'string' && /^7\d+$/.test(v)), inc, ids: [...ids] });
    return ids.map((achievementId) => ({ id: `p-${achievementId}`, achievementId, progress: inc, tier: 0, almostNotified: false }));
  };
  const ctx = (userId) => ({ guildId: gid, guild: {}, getSettings: async () => ({}), message: { author: { id: userId } }, member: null, channel: null });
  try {
    for (let i = 0; i < 3; i++) await svc.onMessageCreate(ctx('70001'));
    await svc.onMessageCreate(ctx('70002'));
    assert.deepEqual(updates, [], 'nothing is written while the messages wait');
    await sleep(120);
    assert.deepEqual(updates.map((u) => [u.user, u.inc]).sort(), [['70001', 3], ['70002', 1]], 'one UPDATE per member, +n');

    updates.length = 0;
    for (let i = 0; i < 5; i++) await svc.onMessageCreate(ctx('70001'));
    await sleep(10);
    assert.deepEqual(updates.map((u) => u.inc), [5], 'written at once when `at` messages wait');

    updates.length = 0;
    await svc.onMessageCreate(ctx('70001'));
    rows = [ach('a1'), ach('a2')]; // a new achievement: later messages count for both, the waiting one for a1 only
    await svc.refreshAchievementCache(gid);
    await svc.onMessageCreate(ctx('70001'));
    await svc.onMessageCreate(ctx('70001'));
    await svc.flushMessageCounts();
    assert.deepEqual(updates.map((u) => [u.inc, u.ids.join()]), [[1, 'a1'], [2, 'a1,a2']]);

    updates.length = 0;
    await svc.onMessageCreate(ctx('70003'));
    rows = []; // deleted before the flush: nothing is written for it
    await svc.refreshAchievementCache(gid);
    await svc.flushMessageCounts();
    assert.deepEqual(updates, []);

    // A reset writes the member's waiting messages first, so they cannot land on the reset row afterwards.
    rows = [ach('a1')];
    await svc.refreshAchievementCache(gid);
    await svc.onMessageCreate(ctx('70004'));
    await svc.onMessageCreate(ctx('70004'));
    await svc.resetMemberProgress(gid, '70004', ['a1']);
    assert.deepEqual(updates.map((u) => (u.reset ? 'reset' : u.inc)), [2, 'reset']);

    // The final flush (shutdown): later messages are written at once, no timer is left to fire.
    updates.length = 0;
    await svc.onMessageCreate(ctx('70005'));
    await svc.flushMessageCounts({ final: true });
    await svc.onMessageCreate(ctx('70005'));
    assert.deepEqual(updates.map((u) => u.inc), [1, 1]);
  } finally {
    prisma.achievement.findMany = real.findMany;
    prisma.$queryRaw = real.queryRaw;
    prisma.guildSettings.upsert = real.upsert;
    prisma.$transaction = real.tx;
    svc.__setMessageFlush({ delayMs: 0 });
    svc.__resetAchievementState();
  }
});

test('DB: messages count atomically, unlock each tier once, pay the rewards and announce', { skip: skipDb }, async () => {
  const gid = G(1);
  resetGrants();
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  addRole(guild, R1);
  const member = addMember(guild, '70001');
  const a = await seedAchievement(gid, {
    tiers: [{ goal: 3, rewards: [rw('ROLE_ADD', R1), rw('XP', 50), rw('COINS', 7)] }, { goal: 5, rewards: [rw('XP', 25)] }],
  });
  const settings = SETTINGS();
  const ctx = msgCtx(guild, settings, member, channel);

  await svc.onMessageCreate(ctx);
  await svc.onMessageCreate(ctx);
  assert.equal((await rowOf(a.id, '70001')).progress, 2);
  assert.equal(channel.sent.length, 0);
  await svc.onMessageCreate(ctx);
  await until(() => channel.sent.length === 1);
  let row = await rowOf(a.id, '70001');
  assert.deepEqual([row.progress, row.tier, row.completedAt, row.almostNotified], [3, 1, null, false]);
  assert.equal(channel.sent[0].content, '🏆 <@70001> unlocked **Chatterbox (Bronze)**!');
  assert.deepEqual(channel.sent[0].allowedMentions, { parse: [], users: ['70001'] });
  assert.deepEqual(member.ops, [['add', R1]]);
  assert.deepEqual(xpGrants.map((g) => g.amount), [50]);
  assert.deepEqual(coinGrants.map((g) => g.amount), [7]);
  assert.equal(xpGrants[0].userId, '70001');
  assert.match(xpGrants[0].reason, /^Achievement: Chatterbox \(Bronze\)$/);

  await svc.onMessageCreate(ctx);
  await svc.onMessageCreate(ctx);
  await until(() => channel.sent.length === 2);
  row = await rowOf(a.id, '70001');
  assert.deepEqual([row.progress, row.tier], [5, 2]);
  assert.ok(row.completedAt instanceof Date);
  assert.equal(channel.sent[1].content, '🏆 <@70001> unlocked **Chatterbox (Silver)**!');
  assert.deepEqual(xpGrants.map((g) => g.amount), [50, 25]);
  // Progress keeps counting after the last tier (D10) and nothing is announced again.
  await svc.onMessageCreate(ctx);
  await sleep(60);
  assert.equal((await rowOf(a.id, '70001')).progress, 6);
  assert.equal(channel.sent.length, 2);
});

test('DB: concurrent first events create one row, add every increment and claim the tier exactly once', { skip: skipDb }, async () => {
  const gid = G(2);
  resetGrants();
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70002');
  const a = await seedAchievement(gid, { goals: [5], rewards: [rw('XP', 10)] });
  const ctx = msgCtx(guild, SETTINGS(), member, channel);
  await Promise.all(Array.from({ length: 12 }, () => svc.onMessageCreate(ctx)));
  await until(() => channel.sent.length >= 1);
  await sleep(150);
  const rows = await prisma.achievementProgress.findMany({ where: { achievementId: a.id } });
  assert.equal(rows.length, 1);
  assert.deepEqual([rows[0].progress, rows[0].tier], [12, 1]);
  assert.equal(channel.sent.length, 1);
  assert.equal(xpGrants.length, 1);
});

test('DB: the claim is a compare-and-set, so two racing evaluations pay once', { skip: skipDb }, async () => {
  const gid = G(3);
  resetGrants();
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70003');
  const a = await seedAchievement(gid, { goals: [2], rewards: [rw('XP', 10)] });
  const lite = svc.toLite(a);
  await ensureGuildSettings(gid);
  const row = await prisma.achievementProgress.create({ data: { guildId: gid, achievementId: a.id, userId: '70003', progress: 2 } });
  const ctx = ctxOf(guild, SETTINGS());
  const snapshot = { id: row.id, achievementId: a.id, progress: 2, tier: 0, almostNotified: false };
  await Promise.all([1, 2, 3].map(() => svc.handleRow({ ctx, settings: SETTINGS(), achievement: lite, row: snapshot, userId: '70003', member, channel })));
  await until(() => channel.sent.length >= 1);
  await sleep(100);
  assert.equal(channel.sent.length, 1);
  assert.equal(xpGrants.length, 1);
  assert.equal((await rowOf(a.id, '70003')).tier, 1);
});

test('DB: the first message seeds MESSAGES from UserLevel.messageCount (trackPastProgress) or from the message only', { skip: skipDb }, async () => {
  const gid = G(4);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70004');
  const past = await seedAchievement(gid, { name: 'Past', goals: [100] });
  const fresh = await seedAchievement(gid, { name: 'Fresh', goals: [100], trackPastProgress: false });
  await prisma.userLevel.create({ data: { guildId: gid, userId: '70004', messageCount: 120, level: 4 } });
  await svc.onMessageCreate(msgCtx(guild, SETTINGS(), member, channel));
  await until(() => channel.sent.length === 1);
  const a = await rowOf(past.id, '70004');
  const b = await rowOf(fresh.id, '70004');
  assert.deepEqual([a.progress, a.tier], [120, 1]); // the levels hook may already have counted this message: max, not plus
  assert.deepEqual([b.progress, b.tier], [1, 0]);
  assert.equal(channel.sent[0].content, '🏆 <@70004> unlocked **Past**!');
});

test('DB: LEVEL counts the absolute level, copes with a null member and never goes down', { skip: skipDb }, async () => {
  const gid = G(5);
  resetGrants();
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  addRole(guild, R2);
  const member = addMember(guild, '70005');
  const a = await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', tiers: [{ goal: 5, rewards: [rw('ROLE_ADD', R2)] }, { goal: 10, rewards: [] }] });
  const ctx = ctxOf(guild, SETTINGS(), { userId: '70005', member: null, newLevel: 5, oldLevel: 4, channel, source: 'voice' });
  await svc.onLevelUp(ctx);
  await until(() => channel.sent.length === 1); // the member is fetched when the hook had none
  assert.deepEqual(member.ops, [['add', R2]]);
  await svc.onLevelUp({ ...ctx, newLevel: 12 });
  await until(() => channel.sent.length === 2);
  assert.deepEqual([(await rowOf(a.id, '70005')).progress, (await rowOf(a.id, '70005')).tier], [12, 2]);
  await svc.onLevelUp({ ...ctx, newLevel: 3 });
  assert.equal((await rowOf(a.id, '70005')).progress, 12);
  // Garbage payloads are ignored.
  await svc.onLevelUp({ ...ctx, userId: undefined, member: null });
  await svc.onLevelUp({ ...ctx, newLevel: 'x' });
  await svc.onLevelUp({ ...ctx, member: { id: '70005', user: { bot: true } } });
  assert.equal((await rowOf(a.id, '70005')).progress, 12);
});

test('DB: a LEVEL admin level-up without a channel falls back to the configured channel, otherwise stays silent', { skip: skipDb }, async () => {
  const gid = G(6);
  const guild = makeGuild(gid);
  const fallback = addChannel(guild, 'cfg');
  addMember(guild, '70006');
  await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', goals: [3] });
  const base = { userId: '70006', member: null, newLevel: 3, oldLevel: 0, channel: null, source: 'admin' };
  await svc.onLevelUp(ctxOf(guild, SETTINGS({ achievementChannelId: 'cfg' }), base));
  await until(() => fallback.sent.length === 1);
  const gid2 = G(7);
  const guild2 = makeGuild(gid2);
  const chan2 = addChannel(guild2, 'x');
  addMember(guild2, '70006');
  await seedAchievement(gid2, { name: 'Climber', trigger: 'LEVEL', goals: [3] });
  await svc.onLevelUp(ctxOf(guild2, SETTINGS(), base));
  await sleep(150);
  assert.equal(chan2.sent.length, 0); // D7: no channel to announce in
  const a = await prisma.achievement.findFirst({ where: { guildId: gid2 } });
  assert.equal((await rowOf(a.id, '70006')).tier, 1); // but the tier was still claimed
});

test('DB: the first message catches LEVEL and BOOST up once per member', { skip: skipDb }, async () => {
  const gid = G(8);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70008', { premium: Date.now() - 86_400_000 });
  const lvl = await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', goals: [10] });
  const boost = await seedAchievement(gid, { name: 'Booster', trigger: 'BOOST', goals: [1] });
  const noPast = await seedAchievement(gid, { name: 'NoPast', trigger: 'LEVEL', goals: [10], trackPastProgress: false });
  await prisma.userLevel.create({ data: { guildId: gid, userId: '70008', level: 12 } });
  const ctx = msgCtx(guild, SETTINGS(), member, channel);
  await svc.onMessageCreate(ctx);
  await until(() => channel.sent.length === 2);
  assert.deepEqual([(await rowOf(lvl.id, '70008')).progress, (await rowOf(lvl.id, '70008')).tier], [12, 1]);
  assert.deepEqual([(await rowOf(boost.id, '70008')).progress, (await rowOf(boost.id, '70008')).tier], [1, 1]);
  assert.equal(await rowOf(noPast.id, '70008'), null); // D4: no catch-up without trackPastProgress
  // Marked as seeded: a deleted row is not re-created by later messages.
  await prisma.achievementProgress.deleteMany({ where: { achievementId: lvl.id } });
  await svc.onMessageCreate(ctx);
  assert.equal(await rowOf(lvl.id, '70008'), null);
  // A member with no history gets no row.
  const other = addMember(guild, '70018');
  await svc.onMessageCreate(msgCtx(guild, SETTINGS(), other, channel));
  assert.equal(await rowOf(boost.id, '70018'), null);
  assert.equal(await rowOf(lvl.id, '70018'), null);
});

test('DB: reactions count once per (message, emoji) and fail closed when Redis is down', { skip: skipDb }, async () => {
  const gid = G(9);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  addMember(guild, '70009');
  const a = await seedAchievement(gid, { name: 'Reactor', trigger: 'REACTIONS', goals: [2] });
  const user = { id: '70009', bot: false };
  const ctx = ctxOf(guild, SETTINGS(), { user, message: { id: '5551', channelId: 'c1' }, emojiKey: '⭐' });
  await svc.onReactionAdd(ctx); // Redis is unreachable here: setNX is null, nothing counts
  assert.equal(await rowOf(a.id, '70009'), null);
  const keys = [];
  cache.setNX = async (key) => { keys.push(key); return true; };
  await svc.onReactionAdd(ctx);
  assert.equal((await rowOf(a.id, '70009')).progress, 1);
  assert.equal(keys[0], `ach:react:${gid}:70009:5551:⭐`);
  cache.setNX = async () => false; // already counted
  await svc.onReactionAdd(ctx);
  assert.equal((await rowOf(a.id, '70009')).progress, 1);
  cache.setNX = async () => true;
  await svc.onReactionAdd({ ...ctx, message: { id: '5552', channelId: 'c1' } });
  await until(() => channel.sent.length === 1);
  assert.equal((await rowOf(a.id, '70009')).tier, 1);
  cache.setNX = realSetNX;
});

test('DB: boosts count once per boost start; an uncertain transition is not counted when Redis is down', { skip: skipDb }, async () => {
  const gid = G(10);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'cfg');
  const member = addMember(guild, '70010', { premium: Date.now() });
  const a = await seedAchievement(gid, { name: 'Booster', trigger: 'BOOST', goals: [1, 2] });
  const settings = SETTINGS({ achievementChannelId: 'cfg' });
  const ctx = (extra) => ctxOf(guild, settings, { newMember: member, oldMember: { premiumSinceTimestamp: null }, oldKnown: true, ...extra });
  await svc.onMemberUpdate(ctx({ oldKnown: false })); // Redis down + heuristic path: fail closed
  assert.equal(await rowOf(a.id, '70010'), null);
  await svc.onMemberUpdate(ctx()); // Redis down but the transition is certain: counts
  assert.equal((await rowOf(a.id, '70010')).progress, 1);
  await until(() => channel.sent.length === 1); // no source channel: announced in the configured channel
  cache.setNX = async () => false;
  await svc.onMemberUpdate(ctx());
  assert.equal((await rowOf(a.id, '70010')).progress, 1);
  cache.setNX = realSetNX;
});

test('DB: purchases add the quantity (bounded) and announce in the purchase channel', { skip: skipDb }, async () => {
  const gid = G(11);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'shop');
  const member = addMember(guild, '70011');
  const a = await seedAchievement(gid, { name: 'Shopper', trigger: 'PURCHASES', goals: [3] });
  const ctx = (extra) => ctxOf(guild, SETTINGS(), { userId: '70011', member, channel, item: {}, totalPrice: 1, ...extra });
  await svc.onPurchase(ctx({ quantity: 2 }));
  assert.equal((await rowOf(a.id, '70011')).progress, 2);
  await svc.onPurchase(ctx({ quantity: 0 })); // not a positive integer: counts as one
  await until(() => channel.sent.length === 1);
  assert.equal((await rowOf(a.id, '70011')).tier, 1);
  await svc.onPurchase(ctx({ quantity: 5_000_000 }));
  assert.equal((await rowOf(a.id, '70011')).progress, 3 + svc.MAX_PURCHASE_DELTA);
  await svc.onPurchase(ctx({ userId: undefined, member: null })); // nothing to attribute it to
});

test('DB: the almost-there DM comes once at 90 percent and the unlock follows by DM', { skip: skipDb }, async () => {
  const gid = G(12);
  const guild = makeGuild(gid);
  const member = addMember(guild, '70012');
  const a = await seedAchievement(gid, { name: 'Talker', goals: [100] });
  const settings = SETTINGS({ achievementAnnounceMode: 'DM' });
  const ctx = ctxOf(guild, settings);
  const lite = svc.toLite(a);
  const rec = (inc) => svc.recordProgress({ ctx, settings, userId: '70012', member, trigger: 'MESSAGES', achievements: [lite], inc });
  await rec(89);
  await sleep(60);
  assert.equal(member.dms.length, 0);
  await rec(1); // 90
  await until(() => member.dms.length === 1);
  assert.equal(member.dms[0].content, "🔥 Almost there! You're at **90/100** for **Talker** in **Test *Guild***.");
  assert.deepEqual(member.dms[0].allowedMentions, { parse: [] });
  await rec(2);
  await sleep(60);
  assert.equal(member.dms.length, 1);
  assert.equal((await rowOf(a.id, '70012')).almostNotified, true);
  await rec(8); // 100
  await until(() => member.dms.length === 2);
  assert.match(member.dms[1].content, /^🏆 <@70012> unlocked \*\*Talker\*\*!\n-# Sent from \*\*Test \\\*Guild\\\*\*\*$/);
  const row = await rowOf(a.id, '70012');
  assert.deepEqual([row.tier, row.almostNotified], [1, false]);
  // A user who blocks DMs (50007) is swallowed silently.
  const blocked = addMember(guild, '70112');
  blocked.send = async () => { throw Object.assign(new Error('Cannot send messages to this user'), { code: 50007 }); };
  blocked.user.send = blocked.send;
  await svc.recordProgress({ ctx, settings, userId: '70112', member: blocked, trigger: 'MESSAGES', achievements: [lite], inc: 100 });
  await sleep(100);
  assert.equal((await rowOf(a.id, '70112')).tier, 1);
});

test('DB: embed announcements, custom text, OFF mode and an unusable channel', { skip: skipDb }, async () => {
  const gid = G(13);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const broken = addChannel(guild, 'c2', { usable: false });
  const member = addMember(guild, '70013');
  const ach = await seedAchievement(gid, { name: 'Embeddy', goals: [1], rewards: [rw('XP', 10)] });
  const lite = svc.toLite(ach);
  const celebrate = (settings, ch = channel, who = member) => svc.announceUnlock({
    guild, member: who, achievement: lite, tierIndex: 0, progress: 1, rewardsText: '+10 XP', channel: ch, settings,
  });
  await celebrate(SETTINGS({ achievementUseEmbed: true, achievementMessage: '{player.name} got {achievement} ({rewards})', embedColor: 0xff0000 }));
  const embedMsg = channel.sent.at(-1);
  assert.equal(embedMsg.content, undefined);
  assert.equal(embedMsg.embeds.length, 1);
  assert.equal(embedMsg.embeds[0].data.description, 'User 70013 got Embeddy (+10 XP)');
  assert.equal(embedMsg.embeds[0].data.color, 0xff0000);
  await celebrate(SETTINGS({ achievementMessage: '  {player} → {tier} {goal}  ' }));
  assert.equal(channel.sent.at(-1).content, '<@70013> → Bronze 1');
  const before = channel.sent.length;
  await celebrate(SETTINGS({ achievementAnnounceMode: 'OFF' }));
  assert.equal(channel.sent.length, before);
  await celebrate(SETTINGS(), broken); // the source is unusable and nothing is configured: skipped
  assert.equal(broken.sent.length, 0);
  await celebrate(SETTINGS({ achievementAnnounceMode: 'CHANNEL', achievementChannelId: 'c1' }), broken); // CHANNEL: configured first
  assert.equal(channel.sent.length, before + 1);
  // DM + embed: the embed rides along with the footer line.
  await celebrate(SETTINGS({ achievementAnnounceMode: 'DM', achievementUseEmbed: true }));
  assert.equal(member.dms.at(-1).embeds.length, 1);
  assert.match(member.dms.at(-1).content, /^-# Sent from/);
  // Send failures never throw.
  const mute = addChannel(guild, 'c3', { failWith: 50013 });
  await celebrate(SETTINGS(), mute);
  const gone = addChannel(guild, 'c4', { failWith: 10003 });
  await celebrate(SETTINGS(), gone);
});

test('embed announcements put a scoped ping in the message content when the text pings the member (like level-ups)', async () => {
  const guild = makeGuild(G(60));
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70060');
  const lite = svc.toLite({
    id: 'x', name: 'Pinger', trigger: 'MESSAGES', trackPastProgress: true, deadline: null, tiers: [{ goal: 1, rewards: [] }],
  });
  const announce = (settings, ch = channel) => svc.announceUnlock({
    guild, member, achievement: lite, tierIndex: 0, progress: 1, rewardsText: '', channel: ch, settings,
  });
  const last = () => channel.sent.at(-1);

  // The default text pings {player}: a mention inside an embed never notifies, so it also goes in the content.
  await announce(SETTINGS({ achievementUseEmbed: true }));
  assert.equal(channel.sent.length, 1);
  assert.equal(last().content, '<@70060>');
  assert.deepEqual(last().allowedMentions, { parse: [], users: ['70060'] });
  assert.match(last().embeds[0].data.description, /<@70060> unlocked \*\*Pinger/);

  // Every spelling of the member mention counts; their casing too.
  for (const token of ['{mention}', '{user}', '{user.mention}', '{PLAYER}']) {
    await announce(SETTINGS({ achievementUseEmbed: true, achievementMessage: `GG ${token}` }));
    assert.equal(last().content, '<@70060>', token);
  }

  // No ping in the text, no ping in the content (same rule as levels).
  await announce(SETTINGS({ achievementUseEmbed: true, achievementMessage: '{player.name} got {achievement}' }));
  assert.equal(last().content, undefined);
  assert.deepEqual(last().allowedMentions, { parse: [], users: ['70060'] }, 'still scoped to the member');

  // Text mode is unchanged: the text is the content.
  await announce(SETTINGS({ achievementMessage: 'GG {player}' }));
  assert.equal(last().content, 'GG <@70060>');
  assert.equal(last().embeds, undefined);

  // An embed that renders to nothing is not sent at all.
  const before = channel.sent.length;
  await announce(SETTINGS({ achievementUseEmbed: true, achievementMessage: '{rewards}' }));
  assert.equal(channel.sent.length, before, 'empty embed: nothing sent');

  // DM mode: the ping would sit next to the "Sent from" footer, so the DM keeps no content mention.
  await announce(SETTINGS({ achievementAnnounceMode: 'DM', achievementUseEmbed: true }));
  const dm = member.dms.at(-1);
  assert.equal(dm.embeds.length, 1);
  assert.match(dm.content, /^-# Sent from/);
  assert.doesNotMatch(dm.content, /<@70060>/);
  assert.deepEqual(dm.allowedMentions, { parse: [] });

  assert.equal(svc.templateMentionsPlayer('{player.name} won'), false);
  assert.equal(svc.templateMentionsPlayer(null), false);
  assert.equal(svc.templateMentionsPlayer('hi {user}'), true);
});

test('DB: payout skips unassignable roles, held roles and removes roles; a reward that cannot be paid is not retried', { skip: skipDb }, async () => {
  const gid = G(14);
  resetGrants();
  const guild = makeGuild(gid);
  addRole(guild, R1);
  addRole(guild, R2);
  const high = '123456789012345699';
  addRole(guild, high, { position: 500 }); // above the bot
  const member = addMember(guild, '70014', { roles: [R2] });
  const a = svc.toLite({
    id: 'x', name: 'Payer', trigger: 'MESSAGES', trackPastProgress: true, deadline: null,
    tiers: [
      { goal: 1, rewards: [rw('ROLE_ADD', R1), rw('ROLE_REMOVE', R2), rw('XP', 5)] },
      { goal: 2, rewards: [rw('ROLE_ADD', high), rw('ROLE_ADD', R1), rw('XP', 6), rw('COINS', 9)] },
    ],
  });
  const lines = await svc.payTierRewards({ guild, member, userId: '70014', achievement: a, fromTier: 0, toTier: 2, channel: null, settings: SETTINGS({ currencyName: 'gold', currencyEmoji: '💰' }) });
  assert.deepEqual(member.ops, [['add', R1], ['remove', R2]]); // `high` is refused, R1 is already held at tier 2
  assert.deepEqual(lines, [`<@&${R1}>`, `−<@&${R2}>`, '+11 XP', '+9 💰 gold']);
  assert.deepEqual(xpGrants.map((g) => g.amount), [11]); // summed across the tiers
  // A member who left: XP and coins still go out, roles are skipped.
  resetGrants();
  const out = await svc.payTierRewards({ guild, member: null, userId: '70099', achievement: a, fromTier: 0, toTier: 1, settings: SETTINGS() });
  assert.deepEqual(out, ['+5 XP']);
  // A rejected role edit is logged, not thrown.
  const flaky = addMember(guild, '70114');
  flaky.roles.add = async () => { throw Object.assign(new Error('Missing Permissions'), { code: 50013 }); };
  assert.deepEqual(await svc.payTierRewards({ guild, member: flaky, userId: '70114', achievement: a, fromTier: 0, toTier: 1, settings: SETTINGS() }), ['+5 XP']);
});

test('DB: a provider that refuses an XP reward does not stop the rest of the payout', { skip: skipDb }, async () => {
  const guild = makeGuild(G(27));
  const a = svc.toLite({ id: 'x', name: 'Refused', trigger: 'MESSAGES', tiers: [{ goal: 1, rewards: [rw('XP', 5), rw('COINS', 3)] }] });
  xpProviderResult.value = { ok: false, reason: 'module-disabled' };
  try {
    const lines = await svc.payTierRewards({ guild, member: null, userId: '70027', achievement: a, fromTier: 0, toTier: 1, settings: SETTINGS() });
    assert.deepEqual(lines, ['+3 🪙 coins']);
  } finally {
    xpProviderResult.value = null;
  }
});

test('DB: reset zeroes progress, keeps the rows and is not re-seeded from history', { skip: skipDb }, async () => {
  const gid = G(15);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const member = addMember(guild, '70015');
  const a = await seedAchievement(gid, { goals: [100, 200] });
  const lvl = await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', goals: [5] });
  await prisma.userLevel.create({ data: { guildId: gid, userId: '70015', messageCount: 150, level: 7 } });
  const ctx = msgCtx(guild, SETTINGS(), member, channel);
  await svc.onMessageCreate(ctx);
  await until(() => channel.sent.length === 2);
  assert.equal((await rowOf(a.id, '70015')).tier, 1);
  await svc.resetMemberProgress(gid, '70015', [a.id, lvl.id]);
  let row = await rowOf(a.id, '70015');
  assert.deepEqual([row.progress, row.tier, row.almostNotified, row.completedAt], [0, 0, false, null]);
  assert.deepEqual([(await rowOf(lvl.id, '70015')).progress, (await rowOf(lvl.id, '70015')).tier], [0, 0]);
  await svc.onMessageCreate(ctx);
  await sleep(100);
  row = await rowOf(a.id, '70015');
  assert.deepEqual([row.progress, row.tier], [1, 0]); // counts from zero, history is not replayed
  assert.equal(channel.sent.length, 2);
  // Resetting a member who never had a row creates zero rows, so history cannot be replayed for them either.
  await svc.resetMemberProgress(gid, '70016', [a.id]);
  assert.deepEqual([(await rowOf(a.id, '70016')).progress, (await rowOf(a.id, '70016')).tier], [0, 0]);
  await svc.resetMemberProgress(gid, '70016', []);
  // A level-up afterwards unlocks the LEVEL achievement again (documented behaviour).
  await svc.onLevelUp(ctxOf(guild, SETTINGS(), { userId: '70015', member, newLevel: 8, oldLevel: 7, channel, source: 'message' }));
  await until(() => channel.sent.length === 3);
  assert.equal((await rowOf(lvl.id, '70015')).tier, 1);
});

// ── Index, CRUD and data functions ────────────────────────────────────────────

test('DB: the index follows config writes, skips disabled and expired achievements', { skip: skipDb }, async () => {
  const gid = G(16);
  svc.__resetAchievementState();
  await ensureGuildSettings(gid);
  assert.deepEqual((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES, []);
  const a = await seedAchievement(gid, { goals: [3] });
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 1);
  assert.equal(svc.mayHaveTriggers(gid, ['MESSAGES']), true);
  assert.equal(svc.mayHaveTriggers(gid, ['BOOST']), false);
  await svc.updateAchievement(gid, a.id, { enabled: false });
  await svc.refreshAchievementCache(gid);
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 0);
  assert.equal(svc.mayHaveTriggers(gid, ['MESSAGES', 'LEVEL', 'BOOST']), false);
  // An expired deadline is filtered lazily; the row stays in the index.
  await svc.updateAchievement(gid, a.id, { enabled: true, deadline: new Date(Date.now() + 60_000) });
  await svc.refreshAchievementCache(gid);
  const [lite] = (await svc.getAchievementIndex(gid)).byTrigger.MESSAGES;
  assert.equal(svc.activeAchievements([lite], Date.now()).length, 1);
  assert.equal(svc.activeAchievements([lite], Date.now() + 120_000).length, 0);
  // A fresh process memo is reused without touching the database.
  const first = await svc.getAchievementIndex(gid);
  assert.equal(await svc.getAchievementIndex(gid), first);
});

test('DB: createAchievement enforces the cap and case-insensitive names; resolve and search', { skip: skipDb }, async () => {
  const gid = G(17);
  await ensureGuildSettings(gid);
  const tiers = [{ goal: 1, rewards: [] }];
  const one = await svc.createAchievement(gid, { name: 'Alpha', trigger: 'MESSAGES', tiers });
  assert.equal(one.ok, true);
  assert.deepEqual(await svc.createAchievement(gid, { name: 'ALPHA', trigger: 'MESSAGES', tiers }), { ok: false, code: 'name-taken' });
  assert.equal(await svc.nameTaken(gid, 'alpha'), true);
  assert.equal(await svc.nameTaken(gid, 'alpha', one.achievement.id), false);
  assert.deepEqual((await svc.resolveAchievement(gid, one.achievement.id)).name, 'Alpha');
  assert.deepEqual((await svc.resolveAchievement(gid, 'aLpHa')).id, one.achievement.id);
  assert.equal(await svc.resolveAchievement(gid, 'nope'), null);
  assert.equal(await svc.resolveAchievement(gid, ''), null);
  assert.equal(await svc.resolveAchievement(G(18), one.achievement.id), null); // another guild's id
  await svc.createAchievement(gid, { name: 'Beta', trigger: 'BOOST', tiers });
  assert.deepEqual((await svc.searchAchievements(gid, 'ET')).map((a) => a.name), ['Beta']);
  assert.deepEqual((await svc.searchAchievements(gid, '')).map((a) => a.name), ['Alpha', 'Beta']);
  assert.deepEqual((await svc.updateAchievement(gid, one.achievement.id, { name: 'Beta' })), { ok: false, code: 'name-taken' }); // unique index
  assert.deepEqual(await svc.updateAchievement(gid, 'missing', { name: 'Zed' }), { ok: false, code: 'not-found' });
  // Stored tiers are always the normalised form.
  const stored = await prisma.achievement.create({ data: { guildId: gid, name: 'Raw', trigger: 'MESSAGES', tiers: [{ goal: 5 }, { goal: 5 }] } });
  assert.deepEqual(svc.toLite(stored).tiers.map((t) => t.goal), [5]);
  // The cap.
  const gid2 = G(19);
  await ensureGuildSettings(gid2);
  await prisma.achievement.createMany({ data: Array.from({ length: LIMITS.ACHIEVEMENTS }, (_, i) => ({ guildId: gid2, name: `A${i}`, trigger: 'MESSAGES', tiers })) });
  assert.deepEqual(await svc.createAchievement(gid2, { name: 'One too many', trigger: 'MESSAGES', tiers }), { ok: false, code: 'limit' });
  assert.equal(await svc.deleteAchievement(gid2, (await prisma.achievement.findFirst({ where: { guildId: gid2 } })).id), 1);
  assert.equal(await svc.deleteAchievement(gid2, 'missing'), 0);
});

test('DB: member view, tier stats and tier changes', { skip: skipDb }, async () => {
  const gid = G(20);
  await ensureGuildSettings(gid);
  const a = await seedAchievement(gid, { name: 'Three', goals: [10, 20, 30] });
  const off = await seedAchievement(gid, { name: 'Off', goals: [1] });
  const hidden = await seedAchievement(gid, { name: 'Hidden', goals: [1] });
  await prisma.achievement.updateMany({ where: { id: { in: [off.id, hidden.id] } }, data: { enabled: false } });
  const mk = (userId, tier, progress, completed = false) => prisma.achievementProgress.create({
    data: { guildId: gid, achievementId: a.id, userId, tier, progress, completedAt: completed ? new Date() : null, almostNotified: true },
  });
  await mk('u0', 0, 1);
  await mk('u1', 1, 12);
  await mk('u2', 2, 25);
  await mk('u3', 3, 99, true);
  await prisma.achievementProgress.create({ data: { guildId: gid, achievementId: off.id, userId: 'u1', tier: 1, progress: 1 } });
  const view = await svc.loadMemberView(gid, 'u1', { premiumSinceTimestamp: null });
  assert.deepEqual(view.entries.map((e) => [e.a.name, e.enabled, e.display.tier]), [['Three', true, 1], ['Off', false, 1]]); // Hidden: disabled and nothing unlocked
  assert.deepEqual((await svc.loadMemberView(gid, 'nobody', null)).entries.map((e) => e.a.name), ['Three']);
  assert.deepEqual(await svc.getTierStats(a.id, 3), { perTier: [3, 2, 1], total: 4 });

  // Remove tier 1 (index 0): members who had passed it lose one count.
  await svc.applyTierChange(a.id, svc.serializeTiers([{ goal: 20 }, { goal: 30 }]), { kind: 'remove', index: 0 });
  const tiers = async () => Object.fromEntries((await prisma.achievementProgress.findMany({ where: { achievementId: a.id } })).map((r) => [r.userId, [r.tier, r.completedAt !== null, r.almostNotified]]));
  assert.deepEqual(await tiers(), { u0: [0, false, false], u1: [0, false, false], u2: [1, false, true], u3: [2, true, true] });
  assert.deepEqual(svc.normalizeTiers((await prisma.achievement.findUnique({ where: { id: a.id } })).tiers).map((t) => t.goal), [20, 30]);
  // Add a tier: finished members are no longer complete.
  await svc.applyTierChange(a.id, svc.serializeTiers([{ goal: 20 }, { goal: 30 }, { goal: 40 }]), { kind: 'add' });
  assert.deepEqual((await tiers()).u3, [2, false, true]);
  // Edit a goal: members waiting on that tier get a fresh almost-there.
  await svc.applyTierChange(a.id, svc.serializeTiers([{ goal: 20 }, { goal: 35 }, { goal: 40 }]), { kind: 'edit', index: 2, goalChanged: true });
  assert.deepEqual((await tiers()).u3, [2, false, false]);
  assert.deepEqual((await tiers()).u2, [1, false, true]);
});

// ── Commands against fake interactions ────────────────────────────────────────

function embedOf(payload) {
  return payload?.embeds?.[0]?.data ?? payload?.embeds?.[0]?.toJSON?.() ?? null;
}

function makeInteraction(guild, { group = null, sub, opts = {}, user, member, channel = null, confirm = 'yes', focused = null } = {}) {
  const replies = [];
  const followUps = [];
  const get = (n) => (opts[n] === undefined ? null : opts[n]);
  let lastPayload = null;
  const message = {
    id: 'msg1',
    createMessageComponentCollector: () => {
      const collector = new EventEmitter();
      const yes = lastPayload.components[0].components[0].data.custom_id;
      const no = lastPayload.components[0].components[1].data.custom_id;
      setImmediate(() => {
        collector.emit('collect', { customId: confirm === 'yes' ? yes : no, update: async () => {} });
        collector.emit('end');
      });
      return collector;
    },
    edit: async () => {},
  };
  const i = {
    guildId: guild.id, guild, user, member, channel, client: guild.client,
    createdTimestamp: Date.now(), deferred: false, replied: false, ephemeral: null,
    replies, followUps, responded: null,
    isRepliable: () => true,
    memberPermissions: { has: () => true },
    options: {
      getSubcommandGroup: () => group,
      getSubcommand: () => sub,
      getString: get, getInteger: get, getBoolean: get, getRole: get, getChannel: get, getUser: get, getMember: get,
      get: (n) => (opts[n] === undefined ? null : { name: n, value: opts[n] }),
      getFocused: () => focused,
    },
    deferReply: async ({ ephemeral } = {}) => { i.deferred = true; i.ephemeral = Boolean(ephemeral); },
    editReply: async (p) => { i.replied = true; lastPayload = p; replies.push(p); return message; },
    followUp: async (p) => { followUps.push(p); return message; },
    respond: async (choices) => { i.responded = choices; },
  };
  return i;
}

async function cmdEnv(n, { settings = {} } = {}) {
  const gid = G(n);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  addChannel(guild, 'c2');
  addChannel(guild, 'bad', { usable: false });
  addRole(guild, R1);
  addRole(guild, R2);
  const admin = addMember(guild, 'owner');
  admin.user.bot = false;
  await ensureGuildSettings(gid);
  await updateSettings(gid, { achievementsEnabled: true, xpEnabled: true, economyEnabled: true, ...settings });
  svc.__resetAchievementState();
  const run = async (opts) => {
    const i = makeInteraction(guild, { user: admin.user, member: admin, channel, ...opts });
    await cfg.execute(i, guild.client);
    return i;
  };
  return { gid, guild, channel, admin, run };
}
const titleOf = (i, n = -1) => embedOf(i.replies.at(n) ?? i.followUps.at(n))?.title;
const textOf = (i) => embedOf(i.replies.at(-1))?.description ?? '';

test('DB: /achievement-config create, edit, enable/disable, list and info', { skip: skipDb }, async () => {
  const { gid, guild, run } = await cmdEnv(21);
  let i = await run({ sub: 'create', opts: { name: '  Chatter  Box ', trigger: 'MESSAGES', goals: '10, 100', role_add: { id: R1 }, xp: 25, description: 'Talk', emoji: '🏆' } });
  assert.equal(i.ephemeral, false); // public
  assert.match(titleOf(i), /Achievement Created/);
  assert.match(textOf(i), /🏆 \*\*Chatter Box\*\* · Messages sent · on/);
  assert.match(textOf(i), /🥉 Bronze — 10 messages → <@&123456789012345601>, \+25 XP/);
  assert.match(textOf(i), /🥈 Silver — 100 messages/);
  assert.match(textOf(i), /These rewards are given at every tier/);
  const row = await prisma.achievement.findFirst({ where: { guildId: gid } });
  assert.equal(row.name, 'Chatter Box');
  assert.deepEqual(row.tiers.map((t) => [t.name, t.goal, t.rewards]), [['Bronze', 10, [rw('ROLE_ADD', R1), rw('XP', 25)]], ['Silver', 100, [rw('ROLE_ADD', R1), rw('XP', 25)]]]);
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 1); // the index was refreshed

  i = await run({ sub: 'create', opts: { name: 'chatter box', trigger: 'REACTIONS', goals: '5' } });
  assert.match(titleOf(i), /Name Taken/);
  i = await run({ sub: 'create', opts: { name: 'X', trigger: 'LEVEL', goals: '5000' } });
  assert.match(titleOf(i), /Invalid Goals/);
  i = await run({ sub: 'create', opts: { name: 'X', trigger: 'LEVEL', goals: '5', emoji: 'nope' } });
  assert.match(titleOf(i), /Invalid Emoji/);
  i = await run({ sub: 'create', opts: { name: 'X', trigger: 'LEVEL', goals: '5', deadline: 'whenever' } });
  assert.match(titleOf(i), /Invalid Deadline/);
  i = await run({ sub: 'create', opts: { name: 'X', trigger: 'LEVEL', goals: '5', role_add: { id: R1 }, role_remove: { id: R1 } } });
  assert.match(textOf(i), /cannot give and take the same role/);
  i = await run({ sub: 'create', opts: { name: '   ', trigger: 'LEVEL', goals: '5' } });
  assert.match(titleOf(i), /Invalid Name/);
  i = await run({ sub: 'create', opts: { name: 'Lvl', trigger: 'LEVEL', goals: '5', deadline: '30d' } });
  assert.match(titleOf(i), /Achievement Created/);
  assert.match(textOf(i), /Deadline: <t:\d+:f>/);
  await updateSettings(gid, { xpEnabled: false });
  i = await run({ sub: 'create', opts: { name: 'NoXp', trigger: 'LEVEL', goals: '5', xp: 5 } });
  assert.match(titleOf(i), /Reward Unavailable/);
  assert.match(textOf(i), /enable leveling/);
  i = await run({ sub: 'create', opts: { name: 'Warn', trigger: 'LEVEL', goals: '5' } });
  assert.match(textOf(i), /Level achievements only progress while the ⭐ Levels module is on/);
  await updateSettings(gid, { xpEnabled: true });

  // edit
  i = await run({ sub: 'edit', opts: { achievement: 'chatter box' } });
  assert.match(titleOf(i), /Nothing To Change/);
  i = await run({ sub: 'edit', opts: { achievement: 'missing', name: 'Y' } });
  assert.match(titleOf(i), /Achievement Not Found/);
  i = await run({ sub: 'edit', opts: { achievement: row.id, name: 'Talker', description: 'none', emoji: 'none', track_past: false, deadline: '10d' } });
  assert.match(titleOf(i), /Achievement Updated/);
  assert.match(textOf(i), /• \*\*Name:\*\* Chatter Box → Talker/);
  assert.match(textOf(i), /• \*\*Description:\*\* Talk → none/);
  assert.match(textOf(i), /• \*\*Badge:\*\* 🏆 → default/);
  assert.match(textOf(i), /• \*\*Deadline:\*\* none → <t:\d+:f>/);
  assert.match(textOf(i), /applies to members who have no progress here yet/);
  let after = await prisma.achievement.findUnique({ where: { id: row.id } });
  assert.deepEqual([after.name, after.description, after.emoji, after.trackPastProgress, after.deadline instanceof Date], ['Talker', null, null, false, true]);
  i = await run({ sub: 'edit', opts: { achievement: row.id, deadline: 'clear' } });
  assert.equal((await prisma.achievement.findUnique({ where: { id: row.id } })).deadline, null);
  i = await run({ sub: 'edit', opts: { achievement: row.id, name: 'lvl' } });
  assert.match(titleOf(i), /Name Taken/);
  i = await run({ sub: 'edit', opts: { achievement: row.id, name: 'TALKER' } }); // renaming to itself (case change) is fine
  assert.match(titleOf(i), /Achievement Updated/);

  // enable / disable
  i = await run({ sub: 'disable', opts: { achievement: 'Talker' } });
  assert.match(titleOf(i), /Achievement Disabled/);
  assert.match(textOf(i), /Progress is kept/);
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 0);
  i = await run({ sub: 'disable', opts: { achievement: 'Talker' } });
  assert.match(titleOf(i), /No Change/);
  i = await run({ sub: 'enable', opts: { achievement: 'Talker' } });
  assert.match(titleOf(i), /Achievement Enabled/);
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 1);

  // list + info
  await updateSettings(gid, { achievementAnnounceMode: 'CHANNEL', achievementChannelId: 'c2' });
  i = await run({ sub: 'list' });
  const list = embedOf(i.replies.at(-1)).description;
  assert.match(list, /^\d+\/50 achievements/);
  assert.match(list, /🏆|💬 \*\*TALKER\*\* · Messages sent · 10 → 100 · ✅ on · 🎁 4/);
  assert.match(list, /Announcements: Custom channel <#c2> · Format: plain text/);
  await updateSettings(gid, { achievementChannelId: 'gone' });
  i = await run({ sub: 'list' });
  assert.match(embedOf(i.replies.at(-1)).description, /⚠️ channel missing/);
  const a = await prisma.achievement.findUnique({ where: { id: row.id } });
  await prisma.achievementProgress.createMany({ data: [
    { guildId: gid, achievementId: a.id, userId: 'm1', tier: 2, progress: 100 },
    { guildId: gid, achievementId: a.id, userId: 'm2', tier: 1, progress: 10 },
    { guildId: gid, achievementId: a.id, userId: 'm3', tier: 0, progress: 1 },
  ] });
  i = await run({ sub: 'info', opts: { achievement: a.id } });
  const data = embedOf(i.replies.at(-1));
  assert.match(data.description, /Members with progress: 3/);
  assert.equal(data.fields.length, 2);
  assert.match(data.fields[0].name, /🥉 Bronze — 10 messages/);
  assert.match(data.fields[0].value, /Unlocked by 2 member\(s\)/);
  assert.match(data.fields[1].value, /Unlocked by 1 member\(s\)/);
  await guild.roles.cache.delete(R1);
  i = await run({ sub: 'info', opts: { achievement: a.id } });
  assert.match(embedOf(i.replies.at(-1)).fields[0].value, /@deleted-role ⚠️/);
  i = await run({ sub: 'list' }); // a module-off note is only on success replies of writes; list is unaffected
  assert.equal(i.replies.length, 1);
});

test('DB: /achievement-config tier add, edit and remove', { skip: skipDb }, async () => {
  const { gid, run } = await cmdEnv(22);
  const a = await seedAchievement(gid, { name: 'Tiered', goals: [10, 50] });
  let i = await run({ group: 'tier', sub: 'add', opts: { achievement: 'Tiered', goal: 50 } });
  assert.match(textOf(i), /must be higher than the top tier's goal \(50\)/);
  i = await run({ group: 'tier', sub: 'add', opts: { achievement: 'Tiered', goal: 200, xp: 5, coins: 3 } });
  assert.match(titleOf(i), /Tier Added/);
  assert.match(textOf(i), /🥇 \*\*Gold\*\* added to \*\*Tiered\*\*/);
  assert.match(textOf(i), /🥇 Gold — 200 messages → \+5 XP, \+3 🪙 coins/);
  i = await run({ group: 'tier', sub: 'add', opts: { achievement: 'Tiered', goal: 300 } });
  assert.match(titleOf(i), /Tier Added/);
  i = await run({ group: 'tier', sub: 'add', opts: { achievement: 'Tiered', goal: 400 } });
  assert.match(titleOf(i), /Too Many Tiers/);

  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 9 } });
  assert.match(titleOf(i), /Unknown Tier/);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 2 } });
  assert.match(titleOf(i), /Nothing To Change/);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 2, goal: 5 } });
  assert.match(textOf(i), /Tier 2 must be above 10 and below 200/);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 4, goal: 150 } });
  assert.match(textOf(i), /Tier 4 must be above 200\./);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 2, goal: 60, role_add: { id: R2 } } });
  assert.match(titleOf(i), /Tier Updated/);
  assert.match(textOf(i), /Members who already unlocked this tier keep it/);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 3, xp: 99 } }); // merges: coins stay
  assert.match(textOf(i), /Gold — 200 messages → \+99 XP, \+3 🪙 coins/);
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 3, clear_rewards: true, goal: 100 } });
  assert.match(textOf(i), /Gold — 100 messages\n/);
  assert.match(textOf(i), /unlock it with their next activity/);
  const stored = svc.normalizeTiers((await prisma.achievement.findUnique({ where: { id: a.id } })).tiers);
  assert.deepEqual(stored.map((t) => [t.goal, t.rewards.length]), [[10, 0], [60, 1], [100, 0], [300, 0]]);

  await prisma.achievementProgress.create({ data: { guildId: gid, achievementId: a.id, userId: 'm1', tier: 3, progress: 120 } });
  i = await run({ group: 'tier', sub: 'remove', opts: { achievement: 'Tiered', tier: 1 } });
  assert.match(titleOf(i), /Tier Removed/);
  assert.equal((await rowOf(a.id, 'm1')).tier, 2);
  i = await run({ group: 'tier', sub: 'remove', opts: { achievement: 'Tiered', tier: 4 } });
  assert.match(titleOf(i), /Unknown Tier/);
  await run({ group: 'tier', sub: 'remove', opts: { achievement: 'Tiered', tier: 1 } });
  await run({ group: 'tier', sub: 'remove', opts: { achievement: 'Tiered', tier: 1 } });
  i = await run({ group: 'tier', sub: 'remove', opts: { achievement: 'Tiered', tier: 1 } });
  assert.match(titleOf(i), /Last Tier/);
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES[0].tiers.length, 1); // refreshed

  // rewards can be refused at configuration time
  i = await run({ group: 'tier', sub: 'edit', opts: { achievement: 'Tiered', tier: 1, role_add: { id: 'ffffffffffffffff01' } } });
  assert.match(titleOf(i), /Role Not Allowed/);
});

test('DB: /achievement-config preset, announce mode, channel and message', { skip: skipDb }, async () => {
  const { gid, run } = await cmdEnv(23, { settings: { achievementsEnabled: false } });
  let i = await run({ sub: 'preset', opts: { template: 'chatterbox' } });
  assert.match(titleOf(i), /Achievement Added/);
  assert.match(textOf(i), /💬 \*\*Chatterbox\*\* · Messages sent · on/);
  assert.match(textOf(i), /Add rewards with `\/achievement-config tier edit`/);
  assert.match(textOf(i), /The Achievements module is off/); // module-off note on success replies
  i = await run({ sub: 'preset', opts: { template: 'chatterbox' } });
  assert.match(textOf(i), /Pass `name:` to add it under another name/);
  i = await run({ sub: 'preset', opts: { template: 'booster', name: 'Server Booster' } });
  assert.match(titleOf(i), /Achievement Added/);
  assert.match(textOf(i), /Boosts happen outside any channel/);
  i = await run({ sub: 'preset', opts: { template: 'level10' } });
  assert.match(textOf(i), /level 10/);
  assert.deepEqual((await prisma.achievement.findMany({ where: { guildId: gid }, orderBy: { createdAt: 'asc' } })).map((a) => a.name), ['Chatterbox', 'Server Booster', 'Level 10']);

  // announce mode
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'CHANNEL' } });
  assert.match(titleOf(i), /Pick a Channel/);
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'CHANNEL', channel: { id: 'bad' } } });
  assert.match(titleOf(i), /Missing Permissions/);
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'CHANNEL', channel: { id: 'c2' }, embed: true } });
  assert.match(titleOf(i), /Announcements Updated/);
  assert.match(textOf(i), /announced in <#c2>/);
  assert.match(textOf(i), /sent as an embed/);
  let s = await prisma.guildSettings.findUnique({ where: { guildId: gid } });
  assert.deepEqual([s.achievementAnnounceMode, s.achievementChannelId, s.achievementUseEmbed], ['CHANNEL', 'c2', true]);
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'DM' } });
  assert.match(textOf(i), /almost there/);
  s = await prisma.guildSettings.findUnique({ where: { guildId: gid } });
  assert.deepEqual([s.achievementAnnounceMode, s.achievementChannelId, s.achievementUseEmbed], ['DM', 'c2', true]); // channel + embed kept
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'CURRENT', embed: false } });
  assert.match(textOf(i), /Events without a channel \(boosts, purchases\) use <#c2>/);
  i = await run({ group: 'announce', sub: 'mode', opts: { mode: 'OFF' } });
  assert.match(textOf(i), /no longer announced/);
  // announce channel
  i = await run({ group: 'announce', sub: 'channel', opts: { channel: { id: 'bad' } } });
  assert.match(titleOf(i), /Missing Permissions/);
  i = await run({ group: 'announce', sub: 'channel', opts: { channel: { id: 'c1' } } });
  assert.match(textOf(i), /Mode switched from Disabled to Custom channel/);
  s = await prisma.guildSettings.findUnique({ where: { guildId: gid } });
  assert.deepEqual([s.achievementAnnounceMode, s.achievementChannelId], ['CHANNEL', 'c1']);
  // announce message
  i = await run({ group: 'announce', sub: 'message', opts: { text: 'GG {player} for {achievement} {tier.emoji}! {nope}' } });
  assert.match(titleOf(i), /Announcement Message Updated/);
  const fields = embedOf(i.replies.at(-1)).fields;
  assert.match(fields[0].value, /GG <@owner> for Chatterbox \(Gold\) 🥇! \{nope\}/);
  assert.equal(fields[1].name, 'Unknown variables');
  assert.match(fields[1].value, /\{nope\}/);
  s = await prisma.guildSettings.findUnique({ where: { guildId: gid } });
  assert.equal(s.achievementMessage, 'GG {player} for {achievement} {tier.emoji}! {nope}');
  i = await run({ group: 'announce', sub: 'message', opts: { text: '{achievement.description}' } });
  assert.match(titleOf(i), /Announcement Message Updated/); // the sample description renders
  i = await run({ group: 'announce', sub: 'message', opts: { text: '{rewards.nope}' } });
  assert.match(titleOf(i), /Announcement Message Updated/);
  i = await run({ group: 'announce', sub: 'message', opts: {} });
  assert.match(titleOf(i), /Announcement Message Reset/);
  assert.equal((await prisma.guildSettings.findUnique({ where: { guildId: gid } })).achievementMessage, null);
});

test('DB: /achievement-config delete and reset ask for confirmation and post publicly', { skip: skipDb }, async () => {
  const { gid, guild, run } = await cmdEnv(24);
  const a = await seedAchievement(gid, { name: 'Doomed', goals: [5] });
  const b = await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', goals: [5] });
  await prisma.achievementProgress.createMany({ data: [
    { guildId: gid, achievementId: a.id, userId: '70024', tier: 1, progress: 9 },
    { guildId: gid, achievementId: b.id, userId: '70024', tier: 1, progress: 9 },
  ] });
  const target = addMember(guild, '70024').user;

  let i = await run({ sub: 'reset', opts: { member: target, achievement: 'Doomed' }, confirm: 'no' });
  assert.equal(i.ephemeral, true);
  assert.equal(i.followUps.length, 0);
  assert.equal((await rowOf(a.id, '70024')).progress, 9);
  i = await run({ sub: 'reset', opts: { member: { ...target, bot: true } } });
  assert.match(titleOf(i), /Not Available/);
  i = await run({ sub: 'reset', opts: { member: target, achievement: 'nope' } });
  assert.match(titleOf(i), /Achievement Not Found/);
  i = await run({ sub: 'reset', opts: { member: target, achievement: 'Doomed' } });
  assert.equal(i.followUps.length, 1);
  assert.equal(i.followUps[0].ephemeral, false); // public result
  assert.match(embedOf(i.followUps[0]).description, /progress on \*\*Doomed\*\* was reset to zero\. Rewards already given were kept\./);
  assert.deepEqual([(await rowOf(a.id, '70024')).progress, (await rowOf(a.id, '70024')).tier], [0, 0]);
  assert.equal((await rowOf(b.id, '70024')).progress, 9);
  i = await run({ sub: 'reset', opts: { member: target } });
  assert.match(embedOf(i.followUps[0]).description, /all 2 achievements/);
  assert.match(embedOf(i.followUps[0]).description, /Level achievements unlock again/);
  assert.equal((await rowOf(b.id, '70024')).progress, 0);

  i = await run({ sub: 'delete', opts: { achievement: 'Doomed' }, confirm: 'no' });
  assert.ok(await prisma.achievement.findUnique({ where: { id: a.id } }));
  i = await run({ sub: 'delete', opts: { achievement: 'Doomed' } });
  assert.equal(i.ephemeral, true);
  assert.equal(i.followUps.length, 1);
  assert.match(embedOf(i.followUps[0]).title, /Achievement Deleted/);
  assert.equal(await prisma.achievement.findUnique({ where: { id: a.id } }), null);
  assert.equal(await rowOf(a.id, '70024'), null); // cascade
  assert.equal((await svc.getAchievementIndex(gid)).byTrigger.MESSAGES.length, 0);
  i = await run({ sub: 'delete', opts: { achievement: 'Doomed' } });
  assert.match(titleOf(i), /Achievement Not Found/);
  await prisma.achievement.deleteMany({ where: { guildId: gid } });
  i = await run({ sub: 'reset', opts: { member: target } });
  assert.match(titleOf(i), /Nothing To Reset/);
});

test('DB: /achievement-config autocomplete searches in the database', { skip: skipDb }, async () => {
  const { gid, guild, admin, channel } = await cmdEnv(25);
  await seedAchievement(gid, { name: 'Alpha Beast', goals: [1], emoji: '🔥' });
  const off = await seedAchievement(gid, { name: 'Beta', trigger: 'BOOST', goals: [1] });
  await svc.updateAchievement(gid, off.id, { enabled: false });
  const ask = async (name, value) => {
    const i = makeInteraction(guild, { user: admin.user, member: admin, channel, focused: { name, value } });
    await cfg.autocomplete(i);
    return i.responded;
  };
  const all = await ask('achievement', '');
  assert.deepEqual(all.map((c) => c.name), ['🔥 Alpha Beast · Messages sent', '💎 Beta · Server boosts (off)']);
  assert.equal(all[0].value.length > 10, true);
  assert.deepEqual((await ask('achievement', 'BEA')).map((c) => c.name), ['🔥 Alpha Beast · Messages sent']);
  assert.deepEqual((await ask('achievement', 'beta')).map((c) => c.name), ['💎 Beta · Server boosts (off)']);
  assert.deepEqual(await ask('other', 'x'), []);
});

test('DB: /achievements shows badges, progress bars and pages', { skip: skipDb }, async () => {
  const gid = G(26);
  const guild = makeGuild(gid);
  const channel = addChannel(guild, 'c1');
  const viewer = addMember(guild, '70026');
  await ensureGuildSettings(gid);
  await updateSettings(gid, { achievementsEnabled: true });
  svc.__resetAchievementState();
  const run = async (opts = {}) => {
    const i = makeInteraction(guild, { user: viewer.user, member: viewer, channel, ...opts });
    await mine.execute(i, guild.client);
    return i;
  };
  let i = await run();
  assert.equal(i.ephemeral, false);
  assert.match(titleOf(i), /No Achievements/);
  assert.match(textOf(i), /Create one with `\/achievement-config preset`/);
  const bot = { id: 'b1', bot: true };
  i = await run({ opts: { member: bot } });
  assert.match(titleOf(i), /Not Available/);

  const a = await seedAchievement(gid, { name: 'Chatterbox', goals: [10, 100], emoji: '🏆', description: 'Talk a lot' });
  await seedAchievement(gid, { name: 'Climber', trigger: 'LEVEL', goals: [5, 10] });
  await prisma.userLevel.create({ data: { guildId: gid, userId: '70026', messageCount: 40, level: 7 } });
  await prisma.achievementProgress.create({ data: { guildId: gid, achievementId: a.id, userId: '70026', tier: 1, progress: 42 } });
  i = await run();
  const data = embedOf(i.replies.at(-1));
  assert.equal(data.title, "🏆 User 70026's achievements");
  assert.equal(data.thumbnail.url, 'https://cdn.example/avatar.png');
  assert.match(data.description, /^\*\*1\/4\*\* tiers unlocked · \*\*0\/2\*\* completed\n\*\*Badges:\*\* 🏆🥉/);
  assert.match(data.description, /🏆 \*\*Chatterbox\*\* — 🥉 Bronze\n▰▰▰▰▱▱▱▱▱▱ 42\/100 → 🥈 Silver\n\*Talk a lot\*/);
  assert.match(data.description, /⭐ \*\*Climber\*\*\n.*7\/10 → 🥈 Silver|⭐ \*\*Climber\*\*\n.*5\/5/); // seeded from the level history
  assert.match(data.description, /unlocks on your next activity/);
  // Another member, not in the server, with a disabled-but-unlocked achievement.
  await prisma.achievement.updateMany({ where: { id: a.id }, data: { enabled: false } });
  i = await run({ opts: { member: { id: '70999', bot: false, username: 'gone', globalName: null, displayAvatarURL: () => 'https://cdn.example/g.png' } } });
  assert.match(titleOf(i), /No Achievements|achievements/);
  i = await run();
  assert.match(embedOf(i.replies.at(-1)).description, /Chatterbox\*\* — 🥉 Bronze \(retired\)/);
  // Many achievements paginate.
  await prisma.achievement.updateMany({ where: { id: a.id }, data: { enabled: true } });
  for (let n = 0; n < 7; n++) await seedAchievement(gid, { name: `Extra ${n}`, goals: [1] });
  i = await run();
  const reply = i.replies.at(-1);
  assert.equal(reply.components.length, 1); // Prev / Stop / Next
  assert.match(embedOf(reply).footer.text, /Page 1 of 2/);
});

test('DB: /achievement-config reset and delete keep working while the module is off and say so', { skip: skipDb }, async () => {
  const { gid, guild, run } = await cmdEnv(28, { settings: { achievementsEnabled: false } });
  const a = await seedAchievement(gid, { name: 'Dormant', goals: [5] });
  await prisma.achievementProgress.create({ data: { guildId: gid, achievementId: a.id, userId: '70028', tier: 1, progress: 9 } });
  const target = addMember(guild, '70028').user;
  let i = await run({ sub: 'reset', opts: { member: target, achievement: 'Dormant' } });
  assert.match(embedOf(i.followUps[0]).description, /was reset to zero/);
  assert.match(embedOf(i.followUps[0]).description, /The Achievements module is off/);
  assert.equal((await rowOf(a.id, '70028')).progress, 0);
  i = await run({ sub: 'delete', opts: { achievement: 'Dormant' } });
  assert.match(embedOf(i.followUps[0]).description, /The Achievements module is off/);
});
