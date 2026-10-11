// Database access for the `achievements` collection (the Achievement table, tiers as JSON). Mirrors /achievement-config:
// the same tier normalisation (achievementService), the same progress fix-ups when tiers change (applyTierChange), and the
// achievement index in Redis (`guild:{g}:achievements`) is rebuilt after every write, so the bot sees a change within its
// 30 s memo. The cap and the unique name are checked in a transaction under a lock on the GuildSettings row (the service's
// own check is count-then-create and its mutex is per process).
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings, getSettings, isModuleEnabled } from '../../services/settingsService.js';
import {
  deleteAchievement, listAchievements, normalizeAchievementName, normalizeRewards, normalizeTiers, refreshAchievementCache,
  serializeTiers, validateTiers,
} from '../../services/achievementService.js';
import { RecordError } from './errors.js';
import { lockGuildSettingsSql } from './levelRoles.store.js';
import { NAME_MAX, TRIGGER_LABELS, collection } from './achievements.js';

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };

const achievementLockSql = (id, guildId) => Prisma.sql`SELECT 1 AS ok FROM "Achievement" WHERE "id" = ${id} AND "guildId" = ${guildId} FOR UPDATE`;

export const RAW_SQL = Object.freeze([
  { name: 'dashboard.lockAchievement', build: () => achievementLockSql('0', '0') },
]);

// ── Tiers: flat dashboard shape <-> the stored { goal, rewards: [{ type, roleId | amount }] } ──────────────

function tierToWire(tier) {
  const reward = (type) => tier.rewards.find((r) => r.type === type);
  return {
    goal: tier.goal,
    roleAdd: reward('ROLE_ADD')?.roleId ?? null,
    roleRemove: reward('ROLE_REMOVE')?.roleId ?? null,
    xp: reward('XP')?.amount ?? null,
    coins: reward('COINS')?.amount ?? null,
  };
}

function tierFromWire(tier) {
  const rewards = [];
  if (tier.roleAdd) rewards.push({ type: 'ROLE_ADD', roleId: tier.roleAdd });
  if (tier.roleRemove) rewards.push({ type: 'ROLE_REMOVE', roleId: tier.roleRemove });
  if (tier.xp) rewards.push({ type: 'XP', amount: tier.xp });
  if (tier.coins) rewards.push({ type: 'COINS', amount: tier.coins });
  return { goal: tier.goal, rewards: normalizeRewards(rewards) };
}

const toItem = (row, members = 0) => ({
  id: row.id,
  name: row.name,
  description: row.description ?? null,
  emoji: row.emoji ?? null,
  trigger: row.trigger,
  tiers: normalizeTiers(row.tiers).map(tierToWire),
  trackPastProgress: row.trackPastProgress !== false,
  deadline: row.deadline ? new Date(row.deadline).toISOString() : null,
  enabled: row.enabled !== false,
  triggerLabel: TRIGGER_LABELS[row.trigger] ?? row.trigger,
  members,
});

/** member counts per achievement id, from one grouped query */
async function memberCounts(guildId) {
  const groups = await prisma.achievementProgress.groupBy({ by: ['achievementId'], where: { guildId }, _count: { _all: true } });
  return new Map(groups.map((g) => [g.achievementId, g._count._all]));
}

const nameTaken = (name) => new RecordError('conflict', `An achievement named "${name}" already exists.`, { path: 'name' });

/**
 * XP and coin rewards only work while their module is on, as in /achievement-config. Only rewards that are new or changed
 * count, so an unrelated edit still saves while the module is off.
 */
async function assertRewardsAvailable(guildId, tiers, stored = []) {
  const before = stored.map(tierToWire);
  const added = (key) => tiers.some((t, i) => t[key] && t[key] !== before[i]?.[key]);
  const wantsXp = added('xp');
  const wantsCoins = added('coins');
  if (!wantsXp && !wantsCoins) return;
  const settings = await getSettings(guildId);
  if (wantsXp && !isModuleEnabled(settings, 'leveling')) {
    throw new RecordError('invalid', 'XP rewards need the Levels module. Turn it on first.', { path: 'tiers' });
  }
  if (wantsCoins && !isModuleEnabled(settings, 'economy')) {
    throw new RecordError('invalid', 'Coin rewards need the Economy module. Turn it on first.', { path: 'tiers' });
  }
}

function cleanName(name) {
  const clean = normalizeAchievementName(name);
  if (!clean) throw new RecordError('invalid', `Names must be 1 to ${NAME_MAX} characters.`, { path: 'name' });
  return clean;
}

function cleanTiers(tiers, trigger) {
  const stored = serializeTiers(tiers.map(tierFromWire));
  const verdict = validateTiers(stored, trigger);
  if (!verdict.ok || stored.length !== tiers.length) {
    throw new RecordError('invalid', verdict.ok ? 'Goals must go up from tier to tier.' : verdict.error, { path: 'tiers' });
  }
  return stored;
}

/**
 * The progress fix-ups of a tier change, compared by position (the single-step rules of applyTierChange, generalised to
 * several steps at once and run in the caller's transaction):
 *  - tiers added at the end: members who had completed everything resume (completedAt cleared);
 *  - tiers removed from the end: members past the new top are clamped to it and counted as completed;
 *  - a changed goal: the "almost there" notice may fire again for members working towards that tier.
 */
async function applyProgressFixups(tx, achievementId, before, after) {
  const progress = tx.achievementProgress;
  if (after.length > before.length) {
    await progress.updateMany({ where: { achievementId, completedAt: { not: null } }, data: { completedAt: null } });
  } else if (after.length < before.length) {
    const n = after.length;
    await progress.updateMany({ where: { achievementId, tier: { gt: n } }, data: { tier: n } });
    await progress.updateMany({ where: { achievementId, tier: { gte: n }, completedAt: null }, data: { completedAt: new Date() } });
    await progress.updateMany({ where: { achievementId, tier: n }, data: { almostNotified: false } });
  }
  for (let i = 0; i < Math.min(before.length, after.length); i++) {
    if (before[i].goal !== after[i].goal) {
      await progress.updateMany({ where: { achievementId, tier: i }, data: { almostNotified: false } });
    }
  }
}

async function getItem(guildId, id) {
  const row = await prisma.achievement.findFirst({ where: { id, guildId } });
  if (!row) return null;
  const members = await prisma.achievementProgress.count({ where: { achievementId: id } });
  return toItem(row, members);
}

export const store = Object.freeze({
  async list(guildId) {
    const [rows, counts] = await Promise.all([listAchievements(guildId), memberCounts(guildId)]);
    return rows.slice(0, LIMITS.ACHIEVEMENTS).map((row) => toItem(row, counts.get(row.id) ?? 0));
  },

  get: getItem,

  async create(guildId, data, ctx = {}) {
    const name = cleanName(data.name);
    const tiers = cleanTiers(data.tiers, data.trigger);
    await assertRewardsAvailable(guildId, data.tiers);
    await ensureGuildSettings(guildId);
    try {
      const row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        if ((await tx.achievement.count({ where: { guildId } })) >= LIMITS.ACHIEVEMENTS) {
          throw new RecordError('limit', `A server can have at most ${LIMITS.ACHIEVEMENTS} achievements. Delete one first.`);
        }
        const clash = await tx.achievement.findFirst({ where: { guildId, name: { equals: name, mode: 'insensitive' } }, select: { id: true } });
        if (clash) throw nameTaken(name);
        return tx.achievement.create({
          data: {
            guildId,
            name,
            description: data.description ?? null,
            emoji: data.emoji ?? null,
            trigger: data.trigger,
            trackPastProgress: data.trackPastProgress,
            deadline: data.deadline ? new Date(data.deadline) : null,
            enabled: data.enabled,
            tiers,
          },
        });
      }, TX_OPTS);
      await refreshAchievementCache(guildId);
      ctx.logger?.info?.(`Dashboard achievement ${row.id} created in ${guildId} by ${ctx.userId}`);
      return toItem(row, 0);
    } catch (err) {
      throw err?.code === 'P2002' ? nameTaken(name) : err;
    }
  },

  async update(guildId, id, data, ctx = {}) {
    const patch = {};
    if (data.name !== undefined) patch.name = cleanName(data.name);
    if (data.description !== undefined) patch.description = data.description;
    if (data.emoji !== undefined) patch.emoji = data.emoji;
    if (data.trackPastProgress !== undefined) patch.trackPastProgress = data.trackPastProgress;
    if (data.deadline !== undefined) patch.deadline = data.deadline ? new Date(data.deadline) : null;
    if (data.enabled !== undefined) patch.enabled = data.enabled;
    if (data.tiers !== undefined) {
      const existing = await prisma.achievement.findFirst({ where: { id, guildId }, select: { tiers: true } });
      if (!existing) throw new RecordError('not_found', 'That achievement no longer exists.');
      await assertRewardsAvailable(guildId, data.tiers, normalizeTiers(existing.tiers));
    }

    try {
      await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(achievementLockSql(id, guildId));
        const current = await tx.achievement.findFirst({ where: { id, guildId } });
        if (!current) throw new RecordError('not_found', 'That achievement no longer exists.');
        // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
        const problem = collection.check(data, { mode: 'update', current: toItem(current) });
        if (problem) throw new RecordError('invalid', problem.message, { path: problem.path });
        if (patch.name !== undefined) {
          const clash = await tx.achievement.findFirst({
            where: { guildId, id: { not: id }, name: { equals: patch.name, mode: 'insensitive' } }, select: { id: true },
          });
          if (clash) throw nameTaken(patch.name);
        }
        if (data.tiers !== undefined) {
          patch.tiers = cleanTiers(data.tiers, current.trigger);
          await applyProgressFixups(tx, id, normalizeTiers(current.tiers), patch.tiers);
        }
        if (Object.keys(patch).length > 0) await tx.achievement.update({ where: { id }, data: patch });
      }, TX_OPTS);
    } catch (err) {
      throw err?.code === 'P2002' ? nameTaken(patch.name ?? '') : err;
    }
    await refreshAchievementCache(guildId);
    ctx.logger?.info?.(`Dashboard achievement ${id} updated in ${guildId} by ${ctx.userId}`);
    return getItem(guildId, id);
  },

  async remove(guildId, id, ctx = {}) {
    if ((await deleteAchievement(guildId, id)) === 0) throw new RecordError('not_found', 'That achievement no longer exists.');
    await refreshAchievementCache(guildId);
    ctx.logger?.info?.(`Dashboard achievement ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
