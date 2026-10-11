// Database access for the `levelRoles` collection (the LevelRole table). Same rules as /levelconfig role add|remove:
// one reward per level, one level per role, at most LIMITS.LEVEL_ROLES per server. Nothing is cached anywhere (the bot
// reads LevelRole rows when a member levels up), so there is nothing to invalidate. Giving the role to members who are
// already past the level needs the gateway (member cache), so it stays a Discord-only step: /levelconfig role sync.
import { Prisma } from '@prisma/client';
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import { ensureGuildSettings } from '../../services/settingsService.js';
import { RecordError } from './errors.js';

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };

/**
 * Locks the GuildSettings row: serialises the count-then-create cap checks of every dashboard store (the bot's
 * in-process mutexes do not span processes). Shared by the other stores of this folder.
 */
export const lockGuildSettingsSql = (guildId) => Prisma.sql`SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = ${guildId} FOR UPDATE`;

export const RAW_SQL = Object.freeze([
  { name: 'dashboard.lockGuildSettings', build: () => lockGuildSettingsSql('0') },
]);

const toItem = (row) => ({ id: row.id, level: row.level, roleId: row.roleId, title: `Level ${row.level}` });

const isUnique = (err) => err?.code === 'P2002';

/** Throws a 'conflict' when the level or the role already has a reward (ignoring the row being edited). */
async function assertFree(tx, guildId, { level, roleId }, exceptId = null) {
  const except = exceptId ? { id: { not: exceptId } } : {};
  const sameLevel = await tx.levelRole.findFirst({ where: { guildId, level, ...except }, select: { roleId: true } });
  if (sameLevel) {
    throw new RecordError('conflict', `Level ${level} already has a reward. Edit that reward instead.`, { path: 'level' });
  }
  const sameRole = await tx.levelRole.findFirst({ where: { guildId, roleId, ...except }, select: { level: true } });
  if (sameRole) {
    throw new RecordError('conflict', `That role is already the reward for level ${sameRole.level}. Remove that reward first, or pick another role.`,
      { path: 'roleId' });
  }
}

const uniqueError = () => new RecordError('conflict', 'That level or role already has a reward.');

export const store = Object.freeze({
  async list(guildId) {
    const rows = await prisma.levelRole.findMany({ where: { guildId }, orderBy: { level: 'asc' }, take: LIMITS.LEVEL_ROLES });
    return rows.map(toItem);
  },

  async get(guildId, id) {
    const row = await prisma.levelRole.findFirst({ where: { id, guildId } });
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx = {}) {
    await ensureGuildSettings(guildId);
    try {
      const row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        await assertFree(tx, guildId, data);
        if ((await tx.levelRole.count({ where: { guildId } })) >= LIMITS.LEVEL_ROLES) {
          throw new RecordError('limit', `You can have at most ${LIMITS.LEVEL_ROLES} level rewards.`);
        }
        return tx.levelRole.create({ data: { guildId, level: data.level, roleId: data.roleId } });
      }, TX_OPTS);
      ctx.logger?.info?.(`Dashboard level reward ${row.id} (level ${row.level}) created in ${guildId} by ${ctx.userId}`);
      return toItem(row);
    } catch (err) {
      throw isUnique(err) ? uniqueError() : err;
    }
  },

  async update(guildId, id, data, ctx = {}) {
    try {
      const row = await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        const current = await tx.levelRole.findFirst({ where: { id, guildId } });
        if (!current) throw new RecordError('not_found', 'That level reward no longer exists.');
        const next = { level: data.level ?? current.level, roleId: data.roleId ?? current.roleId };
        if (next.level === current.level && next.roleId === current.roleId) return current;
        await assertFree(tx, guildId, next, id);
        return tx.levelRole.update({ where: { id }, data: next });
      }, TX_OPTS);
      return toItem(row);
    } catch (err) {
      throw isUnique(err) ? uniqueError() : err;
    }
  },

  async remove(guildId, id, ctx = {}) {
    const { count } = await prisma.levelRole.deleteMany({ where: { id, guildId } });
    if (count === 0) throw new RecordError('not_found', 'That level reward no longer exists.');
    ctx.logger?.info?.(`Dashboard level reward ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
