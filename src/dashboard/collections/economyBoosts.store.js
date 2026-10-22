// Database access for the `economyBoosts` collection (the EconomyBoost table). Create and delete use economyService.js
// (createBoost, deleteBoost), which keep the cap and the case-insensitive unique name under a lock on the GuildSettings
// row. /economy-admin boost has no edit; the dashboard edits name, bonus, role and the on/off switch under the same lock.
// The period is create-only: members' usage rows are keyed by it, so changing it would silently hand out second uses.
// Boosts are not cached anywhere.
import prisma from '../../database/client.js';
import { LIMITS } from '../../config/constants.js';
import {
  EconomyError, createBoost, deleteBoost, listBoosts, setBoostEnabled, validateBoostName,
} from '../../services/economyService.js';
import { RecordError } from './errors.js';
import { lockGuildSettingsSql } from './levelRoles.store.js';

const TX_OPTS = { timeout: 10_000, maxWait: 10_000 };

const PERIOD_TEXT = Object.freeze({ ONE_TIME: 'one-time', DAILY: 'daily', WEEKLY: 'weekly', MONTHLY: 'monthly' });

const toItem = (row) => ({
  id: row.id,
  name: row.name,
  percent: row.percent,
  period: row.period,
  roleId: row.roleId ?? null,
  enabled: row.enabled !== false,
  uses: row._count?.uses ?? 0,
  summary: [`+${row.percent}%`, PERIOD_TEXT[row.period] ?? row.period, row.roleId ? 'one role' : 'everyone', row.enabled === false ? 'off' : null]
    .filter(Boolean).join(' · '),
});

const checkName = (name) => {
  const checked = validateBoostName(name);
  if (!checked.ok) throw new RecordError('invalid', checked.reason, { path: 'name' });
  return checked.value;
};

const nameTaken = (name) => new RecordError('conflict', `A boost named "${name}" already exists.`, { path: 'name' });

function mapError(err, name) {
  if (!(err instanceof EconomyError)) return err;
  if (err.code === 'INVALID' && err.data?.reason === 'limit') {
    return new RecordError('limit', `This server already has ${LIMITS.ECONOMY_BOOSTS} boosts (the maximum).`);
  }
  if (err.code === 'INVALID' && err.data?.reason === 'name-taken') return nameTaken(name);
  if (err.code === 'INVALID') return new RecordError('invalid', 'That boost is not valid.');
  return err;
}

const withUses = (id, guildId) => prisma.economyBoost.findFirst({
  where: { id, guildId }, include: { _count: { select: { uses: true } } },
});

export const store = Object.freeze({
  async list(guildId) {
    return (await listBoosts(guildId)).map(toItem);
  },

  async get(guildId, id) {
    const row = await withUses(id, guildId);
    return row ? toItem(row) : null;
  },

  async create(guildId, data, ctx = {}) {
    const name = checkName(data.name);
    try {
      let row = await createBoost(guildId, {
        name, percent: data.percent, period: data.period, roleId: data.roleId ?? null, createdBy: ctx.userId ?? 'dashboard',
      });
      // createBoost always starts a boost on; /economy-admin boost toggle is the only way to turn it off.
      if (data.enabled === false) row = (await setBoostEnabled(guildId, row.id, false)) ?? row;
      ctx.logger?.info?.(`Dashboard coin boost "${row.name}" +${row.percent}% (${row.period}) created in ${guildId} by ${ctx.userId}`);
      return toItem({ ...row, _count: { uses: 0 } });
    } catch (err) {
      throw mapError(err, name);
    }
  },

  async update(guildId, id, data) {
    const patch = {};
    if (data.name !== undefined) patch.name = checkName(data.name);
    for (const key of ['percent', 'roleId', 'enabled']) if (data[key] !== undefined) patch[key] = data[key];
    try {
      await prisma.$transaction(async (tx) => {
        await tx.$queryRaw(lockGuildSettingsSql(guildId));
        const current = await tx.economyBoost.findFirst({ where: { id, guildId }, select: { id: true } });
        if (!current) throw new RecordError('not_found', 'That boost no longer exists.');
        if (patch.name !== undefined) {
          const clash = await tx.economyBoost.findFirst({
            where: { guildId, id: { not: id }, name: { equals: patch.name, mode: 'insensitive' } }, select: { id: true },
          });
          if (clash) throw nameTaken(patch.name);
        }
        if (Object.keys(patch).length > 0) await tx.economyBoost.update({ where: { id }, data: patch });
      }, TX_OPTS);
    } catch (err) {
      throw err?.code === 'P2002' ? nameTaken(patch.name ?? '') : err;
    }
    return toItem(await withUses(id, guildId));
  },

  async remove(guildId, id, ctx = {}) {
    if (!(await deleteBoost(guildId, id))) throw new RecordError('not_found', 'That boost no longer exists.');
    ctx.logger?.info?.(`Dashboard coin boost ${id} deleted in ${guildId} by ${ctx.userId}`);
  },

  actions: Object.freeze({}),
});
