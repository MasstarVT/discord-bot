// Server Backups service: snapshots of a guild's roles, categories, channels and permission overwrites (stored in
// ServerBackup), the automatic backup job (`backup.auto`) and the restore job (`backup.restore`). The snapshot format and
// the restore engine are in src/utils/guildSnapshot.js; the plugin declaration is src/plugins/backups.js and the command
// src/commands/admin/backup.js.
//
// Restore rules (they are the point of this module):
//   - It NEVER deletes anything. Roles and channels are created, and in mode `full` edited, moved and given permission
//     overwrites one at a time. Things that are not in the backup stay.
//   - Only the SERVER OWNER can start one (checked in the command and again in the button) or delete a backup.
//   - It never runs inside the interaction. Confirm schedules the one-shot job `backup.restore` (dedupe
//     `backup.restore:{guildId}`: one restore at a time per guild). Each run does a bounded batch (about 10 Discord calls,
//     20 s at most) and saves its progress in the job payload, so it resumes after a crash or restart. A batch that was
//     repeated adopts what the crashed one made (see guildSnapshot.adoptRole) instead of making it again.
//   - The bot can only give permissions it has itself and place roles below its own highest role: the rest is masked and
//     reported in the summary posted in the channel where the restore was started.
//   - A finished restore stores old id -> new id in the backup (`data.idMap`, never exported), so restoring the same backup
//     again does not make a second copy of what the first one recreated.
import { ActionRowBuilder, ButtonBuilder, ButtonStyle, PermissionsBitField } from 'discord.js';
import prisma from '../database/client.js';
import { LIMITS } from '../config/constants.js';
import { buildCustomId, parseCustomId } from '../config/customIds.js';
import { isModuleEnabled } from '../config/modules.js';
import { createKeyedMutex, withTimeout } from '../utils/async.js';
import {
  RESTORE_MODES, SnapshotError, captureGuild, exportable, makeEnv, newRestoreState, normaliseRestoreState, parseIdMap, parseSnapshot,
  previewRestore, runRestoreBatch, snapshotBytes, snapshotStats,
} from '../utils/guildSnapshot.js';
import { checkChannelUsable, isInteractionGone, permissionNames, safeReply } from '../utils/interactionHelpers.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { clampText, formatNumber } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import logger from '../utils/logger.js';
import { PermanentJobError, getJob, scheduleJob } from './scheduler.js';
import { ensureGuildSettings, getSettingsSafe } from './settingsService.js';

export { RESTORE_MODES, exportable, parseSnapshot, snapshotStats };

export const BACKUP = Object.freeze({
  NAME_MAX: 64,
  CONFIRM_TTL_MS: 5 * 60_000,         // a restore confirmation expires this long after its message was sent
  AUTO_MAX_WAIT_MS: 60 * 60_000,      // backup.auto wakes at least this often, so dashboard changes take effect
  AUTO_MIN_WAIT_MS: 60_000,
  RETRY_MS: 5 * 60_000,
  STARTUP_DELAY_MS: 60_000,
  STARTUP_JITTER_MS: 5 * 60_000,
  STEP_MS: 2000,                      // between two restore batches
  STALE_MS: 30 * 60_000,              // a restore job this late (the bot was away) is stopped, not resumed weeks later
  ROLE_CAP: 250,                      // Discord's limits, for the preview's warnings
  CHANNEL_CAP: 500,
  CALLS_PER_BATCH: 10,
});

export const autoJobKey = (guildId) => `backup.auto:${guildId}`;
export const restoreJobKey = (guildId) => `backup.restore:${guildId}`;

const LIST_SELECT = Object.freeze({
  id: true, guildId: true, name: true, automatic: true, createdById: true, sizeBytes: true, roleCount: true, channelCount: true, createdAt: true,
});

const clamp = (n, min, max) => Math.min(Math.max(n, min), max);
const isLive = (job) => Boolean(job) && (job.status === 'PENDING' || job.status === 'RUNNING');

// ── Rows ──────────────────────────────────────────────────────────────────────

/** A name typed by an admin: one line, control characters gone, at most NAME_MAX characters; null when empty. */
export function cleanName(name) {
  if (typeof name !== 'string') return null;
  // eslint-disable-next-line no-control-regex
  const flat = name.replace(/[\u0000-\u001f\u007f]+/g, ' ').replace(/\s+/g, ' ').trim();
  return flat ? clampText(flat, BACKUP.NAME_MAX, '') : null;
}

/** `2026-10-21 14:05 UTC` */
export function stampOf(date) {
  const d = date instanceof Date ? date : new Date(date);
  return `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC`;
}

/** How a backup is named in lists and autocomplete. */
export function backupLabel(row) {
  const what = row.name || (row.automatic ? 'Automatic backup' : 'Manual backup');
  return `${what} · ${stampOf(row.createdAt)}${row.name && row.automatic ? ' · auto' : ''}`;
}

export async function listBackups(guildId) {
  return prisma.serverBackup.findMany({
    where: { guildId },
    orderBy: [{ createdAt: 'desc' }, { id: 'desc' }],
    take: LIMITS.BACKUPS_MANUAL + LIMITS.BACKUPS_AUTO_KEEP + 10,
    select: LIST_SELECT,
  });
}

/** One backup of THIS guild (another guild's id is not found). `withData`: the snapshot too. */
export async function getBackup(guildId, id, { withData = false } = {}) {
  if (typeof id !== 'string' || !id || id.length > 64) return null;
  return prisma.serverBackup.findFirst({ where: { id, guildId }, ...(withData ? {} : { select: LIST_SELECT }) });
}

export async function deleteBackup(guildId, id) {
  const { count } = await prisma.serverBackup.deleteMany({ where: { id, guildId } });
  return count;
}

export async function countBackups(guildId) {
  const [manual, automatic] = await Promise.all([
    prisma.serverBackup.count({ where: { guildId, automatic: false } }),
    prisma.serverBackup.count({ where: { guildId, automatic: true } }),
  ]);
  return { manual, automatic };
}

export function newestAutomatic(guildId) {
  return prisma.serverBackup.findFirst({
    where: { guildId, automatic: true }, orderBy: [{ createdAt: 'desc' }, { id: 'desc' }], select: { id: true, createdAt: true },
  });
}

/** Autocomplete rows: name (or kind) and date, newest first. */
export async function searchBackups(guildId, query) {
  const q = String(query ?? '').trim().slice(0, 100);
  return prisma.serverBackup.findMany({
    where: { guildId, ...(q ? { name: { contains: q, mode: 'insensitive' } } : {}) },
    orderBy: [{ createdAt: 'desc' }, { id: 'desc' }],
    take: 25,
    select: LIST_SELECT,
  });
}

/**
 * Deletes the oldest AUTOMATIC backups beyond `keep`. Manual backups are never touched, and neither is `spare` (the one
 * a restore is reading). Returns how many went.
 */
export async function pruneAutomatic(guildId, keep, { spare = [] } = {}) {
  const k = clamp(Number.isInteger(keep) ? keep : 7, 1, LIMITS.BACKUPS_AUTO_KEEP);
  const rows = await prisma.serverBackup.findMany({
    where: { guildId, automatic: true }, orderBy: [{ createdAt: 'desc' }, { id: 'desc' }], skip: k, select: { id: true },
  });
  const ids = rows.map((r) => r.id).filter((id) => !spare.includes(id));
  if (!ids.length) return 0;
  const { count } = await prisma.serverBackup.deleteMany({ where: { guildId, automatic: true, id: { in: ids } } });
  return count;
}

// ── Making a backup ───────────────────────────────────────────────────────────

const createLock = createKeyedMutex();

export async function getRestoreJob(guildId) {
  return getJob(restoreJobKey(guildId));
}

/**
 * Takes a snapshot of the guild (from the cache, no Discord call) and stores it. Manual backups are capped by
 * LIMITS.BACKUPS_MANUAL; one over BACKUP_BYTES serialised is refused; none is made while a restore is running (it would
 * capture a half-restored server).
 * @returns {Promise<{ ok: true, backup: object } | { ok: false, reason: 'restoring'|'cap'|'size', error: string }>}
 */
export async function createBackup({ guild, createdById = null, name = null, automatic = false }) {
  return createLock.run(guild.id, async () => {
    if (isLive(await getRestoreJob(guild.id))) {
      return { ok: false, reason: 'restoring', error: 'A restore is running. Wait until it is finished, then make the backup.' };
    }
    if (!automatic) {
      const manual = await prisma.serverBackup.count({ where: { guildId: guild.id, automatic: false } });
      if (manual >= LIMITS.BACKUPS_MANUAL) {
        return { ok: false, reason: 'cap', error: `This server already has ${LIMITS.BACKUPS_MANUAL} manual backups. Delete one with \`/backup delete\` first (server owner only).` };
      }
    }
    const data = captureGuild(guild);
    const sizeBytes = snapshotBytes(data);
    if (sizeBytes > LIMITS.BACKUP_BYTES) {
      return { ok: false, reason: 'size', error: `The backup would be ${formatNumber(sizeBytes)} bytes, over the limit of ${formatNumber(LIMITS.BACKUP_BYTES)}.` };
    }
    await ensureGuildSettings(guild.id);
    const backup = await prisma.serverBackup.create({
      data: {
        guildId: guild.id, name: cleanName(name), automatic, createdById: automatic ? null : createdById,
        data, sizeBytes, roleCount: data.roles.length, channelCount: data.categories.length + data.channels.length,
      },
      select: LIST_SELECT,
    });
    return { ok: true, backup };
  });
}

// ── backup.auto ───────────────────────────────────────────────────────────────

/**
 * Makes sure the recurring backup.auto job exists: a healthy job is only pulled earlier, a FAILED, DONE or missing one is
 * revived. Never throws. @returns {Promise<boolean>}
 */
export async function ensureAutoJob(guildId, runAt = new Date(Date.now() + BACKUP.AUTO_MAX_WAIT_MS)) {
  try {
    const job = await getJob(autoJobKey(guildId));
    await scheduleJob({
      guildId, type: 'backup.auto', runAt, payload: {}, dedupeKey: autoJobKey(guildId), onConflict: isLive(job) ? 'earliest' : 'replace',
    });
    return true;
  } catch (err) {
    logger.error(`backups: could not schedule backup.auto for guild ${guildId}`, err);
    return false;
  }
}

/** guildReady (always): guilds that never touched the module cost nothing; the others get their job back. */
export async function onGuildReady(ctx) {
  const settings = await ctx.getSettings();
  if (!settings) return;
  if (!isModuleEnabled(settings, 'backups') && !(settings.backupIntervalHours > 0)) return;
  await ensureAutoJob(ctx.guildId, new Date(Date.now() + BACKUP.STARTUP_DELAY_MS + Math.floor(Math.random() * BACKUP.STARTUP_JITTER_MS)));
}

/**
 * The backup.auto job. Reads the settings on every run, so a change made on the dashboard is picked up within an hour (the
 * job never waits longer). Makes an automatic backup when the module is on, the interval is above 0 and the newest
 * automatic backup is older than the interval, then drops the oldest automatic ones beyond `backupKeep`. Never throws, so
 * the recurring job cannot go FAILED.
 */
export async function handleAutoJob(job, ctx) {
  const now = Date.now();
  const again = (ms) => ({ nextRunAt: new Date(now + clamp(ms, BACKUP.AUTO_MIN_WAIT_MS, BACKUP.AUTO_MAX_WAIT_MS)) });
  try {
    const guildId = job.guildId;
    const settings = await getSettingsSafe(guildId);
    if (!settings) return again(BACKUP.RETRY_MS);
    const hours = Number(settings.backupIntervalHours) || 0;
    if (!isModuleEnabled(settings, 'backups') || hours <= 0) return again(BACKUP.AUTO_MAX_WAIT_MS); // idle, but still watching
    if (!ctx.guild?.available) return again(BACKUP.RETRY_MS);
    const restore = await getRestoreJob(guildId);
    if (isLive(restore)) return again(10 * 60_000); // a half-restored server is not worth keeping

    const interval = hours * 3_600_000;
    const newest = await newestAutomatic(guildId);
    const dueAt = newest ? newest.createdAt.getTime() + interval : now;
    if (dueAt > now) return again(dueAt - now);

    const made = await createBackup({ guild: ctx.guild, automatic: true });
    if (!made.ok) {
      logger.warn(`backups: automatic backup of guild ${guildId} was not made: ${made.error}`);
      return again(BACKUP.AUTO_MAX_WAIT_MS);
    }
    const pruned = await pruneAutomatic(guildId, settings.backupKeep, { spare: [restore?.payload?.backupId].filter(Boolean) });
    logger.info(`backups: automatic backup ${made.backup.id} of guild ${guildId} made${pruned ? `, ${pruned} old one(s) removed` : ''}`);
    return again(interval);
  } catch (err) {
    logger.error(`backup.auto failed for guild ${job.guildId}`, err);
    return again(BACKUP.RETRY_MS);
  }
}

// ── Starting a restore ────────────────────────────────────────────────────────

/** Entries of a backup's stored id map whose new role or channel still exists (older restores of the same backup). */
export function seedIdMap(guild, snapshot) {
  const out = {};
  for (const [oldId, newId] of Object.entries(parseIdMap(snapshot.idMap))) {
    if (guild.roles.cache.has(newId) || guild.channels.cache.has(newId)) out[oldId] = newId;
  }
  return out;
}

/**
 * What an UNFINISHED earlier restore of the same backup got done: its job row stays FAILED (an error, or stopped because it
 * waited too long) with the last saved payload. A new restore continues from it (its id map and what it made), so what it
 * saved is not created twice and what it made still gets its overwrites in mode `missing`. A finished restore's row is gone
 * (its ids are in the backup, see rememberIds).
 * @returns {{ idMap: Record<string, string>, made: string[], startedAt: number|null }}
 */
export function carriedFrom(guild, job, backupId) {
  const none = { idMap: {}, made: [], startedAt: null };
  if (!job || job.status !== 'FAILED' || job.payload?.backupId !== backupId) return none;
  const prev = normaliseRestoreState(job.payload);
  if (!prev) return none;
  const exists = (id) => guild.roles.cache.has(id) || guild.channels.cache.has(id);
  const idMap = {};
  for (const [oldId, newId] of Object.entries(prev.idMap)) if (exists(newId)) idMap[oldId] = newId;
  return { idMap, made: prev.made.filter(exists), startedAt: prev.startedAt };
}

/** The restore preview (counts, warnings) of a backup against the guild as it is now. */
export function restorePreview(guild, snapshot, mode, { prevJob = null, backupId = null } = {}) {
  return previewRestore({ guild, snapshot, mode, idMap: { ...seedIdMap(guild, snapshot), ...carriedFrom(guild, prevJob, backupId).idMap } });
}

/**
 * Schedules the restore job. Refuses while one is live (`replace` would reset its progress and make everything twice).
 * @returns {Promise<{ ok: true } | { ok: false, reason: 'running'|'mode' }>}
 */
export async function startRestore({ guild, backupId, snapshot, mode, startedById, channelId }) {
  if (!RESTORE_MODES.includes(mode)) return { ok: false, reason: 'mode' };
  const prev = await getRestoreJob(guild.id);
  if (isLive(prev)) return { ok: false, reason: 'running' };
  const carried = carriedFrom(guild, prev, backupId);
  // The start time is NOT carried: adoption is by name, and a wider window would adopt same-named roles or channels made
  // since by someone else (a booster's "Moderator" role, an admin's #general). Items of the stopped run's last unsaved batch
  // (about 10 calls) may be made again; everything it saved is reused.
  const payload = newRestoreState({ backupId, mode, startedById, channelId, idMap: { ...seedIdMap(guild, snapshot), ...carried.idMap } });
  payload.made = carried.made;
  await scheduleJob({
    guildId: guild.id, type: 'backup.restore', runAt: new Date(Date.now() + BACKUP.STEP_MS), payload,
    dedupeKey: restoreJobKey(guild.id), maxAttempts: 5, onConflict: 'keep',
  });
  return { ok: true };
}

// ── The restore job ───────────────────────────────────────────────────────────

/**
 * The permissions a restore needs that the bot lacks: Manage Roles AND Manage Channels, both, because a channel made
 * without its permission overwrites would be open to everyone. Checked before scheduling and in every batch.
 * @returns {string[]} names, empty when the bot can restore
 */
export function missingRestorePermissions(me) {
  const out = [];
  if (me?.permissions?.has?.(PermissionsBitField.Flags.ManageRoles) !== true) out.push('Manage Roles');
  if (me?.permissions?.has?.(PermissionsBitField.Flags.ManageChannels) !== true) out.push('Manage Channels');
  return out;
}

export const missingPermissionsEmbed = (missing) => errorEmbed('Missing Permissions',
  `I need ${missing.map((p) => `**${p}**`).join(' and ')} to restore a backup: a channel I could not give its permissions would be open to everyone. Give my role ${missing.length > 1 ? 'those permissions' : 'that permission'}, then run \`/backup restore\` again.`);

/** Guilds with a batch in flight in this process: a retry that finds one waits (the old run may still be going on). */
const batching = new Set();

/**
 * The backup.restore job: one bounded batch per run (see the top of this file). Returns `{ nextRunAt, payload }` while there
 * is more to do and nothing when it is finished. Discord errors never escape (they are counted and listed in the summary);
 * a database failure throws, so the scheduler retries the run.
 */
export async function handleRestoreJob(job, ctx) {
  const guildId = job.guildId;
  if (batching.has(guildId)) return { nextRunAt: new Date(Date.now() + BACKUP.STEP_MS) }; // payload unchanged
  batching.add(guildId);
  try {
    return await runRestore(job, ctx);
  } finally {
    batching.delete(guildId);
  }
}

async function runRestore(job, ctx) {
  const state = normaliseRestoreState(job.payload);
  if (!state) throw new PermanentJobError('backup.restore: the job payload is unusable');
  const { guild } = ctx;

  const row = await prisma.serverBackup.findFirst({ where: { id: state.backupId, guildId: job.guildId } }); // a failure throws: retried
  if (!row) {
    await postSummary(ctx, state, { stopped: 'The backup was deleted, so the restore stopped.' });
    return undefined;
  }
  let snapshot;
  try {
    snapshot = parseSnapshot(row.data);
  } catch (err) {
    if (!(err instanceof SnapshotError)) throw err;
    await postSummary(ctx, state, { stopped: err.message });
    return undefined;
  }
  if (ctx.lateByMs > BACKUP.STALE_MS) {
    // Stopped, not finished: the ids are stored in the backup and the job row is left FAILED with its payload, so the next
    // `/backup restore` of this backup continues from it (carriedFrom) instead of making everything a second time.
    await rememberIds(row, snapshot, state, guild);
    await postSummary(ctx, state, { stopped: 'The restore waited too long (the bot was away from this server), so it was stopped. Start it again with `/backup restore`; what was already restored is detected and not made twice.' });
    throw new PermanentJobError('backup.restore: stopped because it waited too long (the bot was away from this server)');
  }

  let me = guild.members?.me ?? null;
  if (!me) me = await withTimeout(guild.members.fetchMe(), 10_000, 'fetch my member').catch(() => null);
  if (!me) return { nextRunAt: new Date(Date.now() + 30_000) }; // cannot judge permissions yet; payload unchanged

  const reason = `Server backup restore ${state.backupId.slice(-6)}${state.startedById ? ` (started by ${state.startedById})` : ''}`;
  const env = makeEnv({ guild, snapshot, state, me, reason });
  const missing = missingRestorePermissions(me);
  if (missing.length) {
    // Without Manage Roles a channel would be made without its permissions (open to everyone): stop instead, resumable
    // like a stale stop (the row stays FAILED with its payload, see carriedFrom).
    await rememberIds(row, snapshot, state, guild);
    await postSummary(ctx, state, { stopped: `I no longer have ${missing.map((p) => `**${p}**`).join(' and ')}, so the restore stopped before making anything it could not protect. Give my role ${missing.length > 1 ? 'those permissions' : 'that permission'} back, then run \`/backup restore\` again; what was already restored is not made twice.` });
    throw new PermanentJobError(`backup.restore: stopped, the bot lacks ${missing.join(' and ')}`);
  }
  const result = await runRestoreBatch(env);
  if (result === 'more') return { nextRunAt: new Date(Date.now() + BACKUP.STEP_MS), payload: state };

  await rememberIds(row, snapshot, state, guild);
  await postSummary(ctx, state, { snapshotName: snapshot.guild.name });
  return undefined;
}

/** Stores old id -> new id in the backup, so a second restore of it adopts these instead of recreating them. */
async function rememberIds(row, snapshot, state, guild) {
  try {
    const merged = {};
    for (const [oldId, newId] of Object.entries({ ...snapshot.idMap, ...state.idMap })) {
      if (guild.roles.cache.has(newId) || guild.channels.cache.has(newId)) merged[oldId] = newId;
    }
    if (!Object.keys(state.idMap).length) return;
    await prisma.serverBackup.update({ where: { id: row.id }, data: { data: { ...row.data, idMap: merged } } });
  } catch (err) {
    logger.warn(`backups: could not store the id map of backup ${row.id}: ${err?.message}`);
  }
}

// ── Summary ───────────────────────────────────────────────────────────────────

const count = (state, key) => state.counts[key] ?? 0;
/** Names of the permission bits in `value`, each one on its own (Administrator does not stand for all of them). */
export const bitNames = (value) => permissionNames(new PermissionsBitField(value).toArray(false));
const part = (label, n) => (n > 0 ? `${label} ${formatNumber(n)}` : null);
const joinParts = (...parts) => parts.filter(Boolean).join(' · ') || 'nothing to do';

/** The embed posted when a restore ends (or is stopped). */
export function buildSummaryEmbed(state, { stopped = null } = {}) {
  const full = state.mode === 'full';
  const title = stopped ? 'Restore Stopped' : 'Restore Finished';
  const embed = (stopped ? warnEmbed : successEmbed)(title,
    `${stopped ? `${stopped}\n\n` : ''}${full ? 'Mode **full**' : 'Mode **missing only**'}. **Nothing was deleted.**`);
  const c = (k) => count(state, k);
  embed.addFields(
    { name: 'Roles', value: joinParts(part('created', c('rolesCreated')), part('updated', c('rolesUpdated')), part('already there', c('rolesKept')), part('skipped', c('rolesSkipped'))) },
    { name: 'Categories', value: joinParts(part('created', c('categoriesCreated')), part('updated', c('categoriesUpdated')), part('already there', c('categoriesKept')), part('skipped', c('categoriesSkipped'))), inline: true },
    { name: 'Channels', value: joinParts(part('created', c('channelsCreated')), part('updated', c('channelsUpdated')), part('already there', c('channelsKept')), part('skipped', c('channelsSkipped'))), inline: true },
    { name: 'Permissions and order', value: joinParts(part('channel permissions set', c('overwritesSet')), part('already right', c('overwritesKept')), part('skipped', c('overwritesSkipped')), part('positions set', c('positionsSet')), part('placed lower than before', c('positionsLimited'))) },
  );
  const dropped = BigInt(state.dropped || '0');
  const notes = [];
  if (c('rolesSkipped') > 0) notes.push('Roles are skipped when they belong to an integration, or (full mode) sit above my highest role.');
  if (c('positionsLimited') > 0) notes.push('A role cannot be placed above my own highest role, so some were placed lower than before. Move my role up, then run the restore again.');
  if (notes.length) embed.addFields({ name: 'Good to know', value: clampText(notes.map((n) => `• ${n}`).join('\n'), 1024) });
  if (c('masked') > 0 && dropped !== 0n) {
    embed.addFields({
      name: 'Permissions I could not give',
      value: clampText(`${formatNumber(c('masked'))} role(s) or channel permission(s) were restored without: ${bitNames(dropped).join(', ')}. I can only give permissions I have myself. Give my role those permissions, then run the restore again.`, 1024),
    });
  }
  if (state.errors.length) {
    const shown = state.errors.slice(0, 8);
    const more = c('errors') - shown.length;
    embed.addFields({
      name: `Problems (${formatNumber(c('errors'))})`,
      value: clampText(`${shown.map((e) => `• ${e}`).join('\n')}${more > 0 ? `\n… and ${formatNumber(more)} more` : ''}`, 1024),
    });
  }
  return embed;
}

/**
 * Posts the summary where the restore was started (the channel may have been recreated by the restore itself); falls back to
 * a DM to whoever started it. Never throws.
 */
export async function postSummary(ctx, state, opts = {}) {
  try {
    const embed = buildSummaryEmbed(state, opts);
    const { guild, client } = ctx;
    const who = state.startedById;
    const payload = { content: who ? `<@${who}>` : undefined, embeds: [embed], allowedMentions: who ? { users: [who] } : { parse: [] } };
    const channel = guild.channels.cache.get(state.channelId) ?? guild.channels.cache.get(state.idMap[state.channelId]) ?? null;
    if (channel && checkChannelUsable(channel).ok) {
      await channel.send(payload);
      return true;
    }
    if (who && client?.users) {
      const user = await client.users.fetch(who);
      await user.send({ embeds: [embed] });
      return true;
    }
  } catch (err) {
    logger.warn(`backups: could not post the restore summary for guild ${ctx?.guild?.id}: ${err?.message}`);
  }
  return false;
}

// ── Embeds for the command ────────────────────────────────────────────────────

export const ownerOnlyEmbed = () => errorEmbed('Server Owner Only',
  'Only the owner of this server can restore or delete backups. A restore rebuilds roles and permissions, so it is kept away from every other account, administrators included.');

const BACKUP_LIMITS_NOTE = 'A backup holds roles, categories, channels and their permission overwrites. Threads, messages, members and their roles, emojis, stickers, webhooks and bot settings are not included.';

export function buildInfoEmbed(settings, row, snapshot) {
  const stats = snapshotStats(snapshot);
  const embed = guildEmbed(settings, `💾 ${row.name || (row.automatic ? 'Automatic backup' : 'Manual backup')}`,
    `${row.automatic ? 'Made automatically' : `Made by ${row.createdById ? `<@${row.createdById}>` : 'someone'}`} ${discordTimestamp(row.createdAt, 'R')}.`);
  embed.addFields(
    { name: 'Contains', value: `${formatNumber(stats.roles)} roles · ${formatNumber(stats.categories)} categories · ${formatNumber(stats.channels)} channels · ${formatNumber(stats.overwrites)} channel permissions`, inline: false },
    { name: 'Size', value: `${formatNumber(row.sizeBytes)} bytes`, inline: true },
    { name: 'Server then', value: clampText(snapshot.guild.name || 'unknown', 100), inline: true },
    { name: 'Not included', value: BACKUP_LIMITS_NOTE },
  );
  embed.setFooter({ text: `ID ${row.id}` });
  return embed;
}

export function buildListEmbed(settings, rows) {
  const lines = rows.map((r) => `${r.automatic ? '🤖' : '👤'} **${clampText(r.name || (r.automatic ? 'Automatic backup' : 'Manual backup'), 64)}** · ${discordTimestamp(r.createdAt, 'R')} · ${formatNumber(r.roleCount)} roles, ${formatNumber(r.channelCount)} channels`);
  return guildEmbed(settings, `💾 Backups (${rows.length})`,
    lines.length ? clampText(lines.join('\n'), 4000) : 'No backups yet. Make one with `/backup create`.', 'Pick one by name with /backup info, export, restore or delete.');
}

function estimate(calls) {
  const seconds = Math.max(1, Math.ceil(calls / BACKUP.CALLS_PER_BATCH)) * 5;
  if (seconds < 60) return 'under a minute';
  const minutes = Math.ceil(seconds / 60);
  return `about ${minutes} minute${minutes === 1 ? '' : 's'}`;
}

/** The confirmation embed of /backup restore: what will happen, in counts. */
export function buildPreviewEmbed(row, p) {
  const full = p.mode === 'full';
  const embed = warnEmbed('Restore This Backup?',
    `Backup **${clampText(row.name || (row.automatic ? 'Automatic backup' : 'Manual backup'), 64)}** from ${discordTimestamp(row.createdAt, 'f')}.\n\n`
    + (full
      ? '**Full:** recreates what is missing, and also sets the names, permissions, order and channel permissions of roles and channels that still exist back to the backup.'
      : '**Missing only:** recreates roles and channels that no longer exist, with their channel permissions. Channels that still exist only get the permissions of recreated roles added where they have none; nothing that exists is changed.')
    + '\n**Nothing is ever deleted:** roles and channels that are not in the backup stay.');
  const row3 = (b) => joinParts(part('create', b.create), full ? part('update', b.update) : null, part('leave', b.keep), part('skip', b.skip));
  embed.addFields(
    { name: 'Roles', value: row3(p.roles) },
    { name: 'Categories', value: row3(p.categories), inline: true },
    { name: 'Channels', value: row3(p.channels), inline: true },
    { name: 'Channel permissions', value: p.overwrites ? `${formatNumber(p.overwrites)} to set` : 'none to set', inline: true },
    { name: 'Time', value: `${estimate(p.calls)}; I post the result here when it is done.`, inline: true },
  );
  const warnings = [];
  if (!p.canManageRoles) warnings.push('I do not have **Manage Roles**: roles and channel permissions will be skipped.');
  if (!p.canManageChannels) warnings.push('I do not have **Manage Channels**: categories and channels will be skipped.');
  if (p.masked > 0 && p.dropped !== 0n) warnings.push(`I lack ${bitNames(p.dropped).join(', ')}, so ${formatNumber(p.masked)} role(s) or channel permission(s) will be restored without them. Give my role those permissions first for a complete restore.`);
  if (p.managedMissing > 0) warnings.push(`${formatNumber(p.managedMissing)} role(s) belong to integrations or bots and cannot be recreated.`);
  if (p.roleTotalAfter > BACKUP.ROLE_CAP) warnings.push(`The server would have ${formatNumber(p.roleTotalAfter)} roles; Discord allows ${BACKUP.ROLE_CAP}, so some will fail.`);
  if (p.channelTotalAfter > BACKUP.CHANNEL_CAP) warnings.push(`The server would have ${formatNumber(p.channelTotalAfter)} channels; Discord allows ${BACKUP.CHANNEL_CAP}, so some will fail.`);
  if (p.unsupported > 0) warnings.push(`${formatNumber(p.unsupported)} channel(s) of a type a backup cannot hold were not saved.`);
  if (warnings.length) embed.addFields({ name: 'Heads up', value: clampText(warnings.map((w) => `• ${w}`).join('\n'), 1024) });
  embed.setFooter({ text: 'Confirm within 5 minutes. Only the server owner can confirm.' });
  return embed;
}

export function restoreButtons(backupId, mode) {
  return new ActionRowBuilder().addComponents(
    new ButtonBuilder().setCustomId(buildCustomId('backup', 'restore', backupId, mode)).setLabel('Restore').setStyle(ButtonStyle.Danger),
    new ButtonBuilder().setCustomId(buildCustomId('backup', 'cancel')).setLabel('Cancel').setStyle(ButtonStyle.Secondary),
  );
}

const PHASE_LABEL = Object.freeze({
  roles: 'roles', rolePositions: 'role order', categories: 'categories', channels: 'channels',
  channelPositions: 'channel order', overwrites: 'channel permissions', finish: 'finishing',
});

/** Text for /backup status: where a running restore is. */
export function restoreStatusText(job) {
  if (!job) return 'No restore is running.';
  const p = normaliseRestoreState(job.payload);
  if (isLive(job) && p) {
    return `🔄 Restoring (${p.mode === 'full' ? 'full' : 'missing only'}), now on **${PHASE_LABEL[p.phase]}**. `
      + `Made so far: ${formatNumber(count(p, 'rolesCreated'))} roles, ${formatNumber(count(p, 'categoriesCreated') + count(p, 'channelsCreated'))} channels. `
      + `Started ${discordTimestamp(p.startedAt, 'R')}${p.startedById ? ` by <@${p.startedById}>` : ''}.`;
  }
  if (job.status === 'FAILED') return `⚠️ The last restore stopped with an error: ${clampText(job.lastError || 'unknown', 200)} Start it again with \`/backup restore\`; what was restored is not made twice.`;
  return 'No restore is running.';
}

export function buildStatusEmbed({ settings, counts, newest, restoreJob, problems }) {
  const hours = Number(settings?.backupIntervalHours) || 0;
  const on = isModuleEnabled(settings, 'backups');
  const keep = clamp(Number.isInteger(settings?.backupKeep) ? settings.backupKeep : 7, 1, LIMITS.BACKUPS_AUTO_KEEP);
  let auto;
  if (hours <= 0) auto = 'Off. Set the interval on the dashboard (Server Backups).';
  else {
    const next = newest ? new Date(newest.createdAt.getTime() + hours * 3_600_000) : null;
    auto = `Every **${hours}** hour${hours === 1 ? '' : 's'}, keeping the newest **${keep}**.`
      + `${newest ? ` Last: ${discordTimestamp(newest.createdAt, 'R')}.` : ' None made yet.'}`
      + `${next && next.getTime() > Date.now() ? ` Next: ${discordTimestamp(next, 'R')}.` : ''}`
      + `${on ? '' : ' **Paused: the module is off** (`/module enable backups`).'}`;
  }
  const embed = guildEmbed(settings, '💾 Server Backups');
  embed.addFields(
    { name: 'Automatic backups', value: auto },
    { name: 'Stored', value: `${formatNumber(counts.manual)} / ${LIMITS.BACKUPS_MANUAL} manual · ${formatNumber(counts.automatic)} automatic`, inline: true },
    { name: 'Restore', value: restoreStatusText(restoreJob) },
  );
  if (problems?.length) embed.addFields({ name: 'Needs attention', value: clampText(problems.map((p) => `• ${p}`).join('\n'), 1024) });
  return embed;
}

// ── The persistent buttons ────────────────────────────────────────────────────

const edit = (interaction, embed) => interaction.editReply({ embeds: [embed], components: [] }).catch((err) => logger.debug(`backups: edit failed: ${err?.message}`));

/**
 * `backup:restore:{backupId}:{mode}` and `backup:cancel` on the confirmation. SERVER OWNER only (checked here, before any
 * await, whatever the command checked); the confirmation expires 5 minutes after its message was sent. Works while the
 * module is off. Never throws.
 */
export async function handleBackupButton(interaction, client) {
  try {
    const { params } = parseCustomId(interaction.customId);
    const action = params[0];
    const guild = interaction.guild ?? client?.guilds?.cache?.get(interaction.guildId) ?? null;
    if (!guild) return void await safeReply(interaction, { embeds: [errorEmbed('Try Again', 'I cannot see this server right now. Please try again in a moment.')], ephemeral: true });
    if (interaction.user.id !== guild.ownerId) return void await safeReply(interaction, { embeds: [ownerOnlyEmbed()], ephemeral: true });

    if (action === 'cancel') {
      try {
        await interaction.update({ embeds: [infoEmbed('Restore Cancelled', 'Nothing was changed.')], components: [] });
      } catch (err) {
        if (!isInteractionGone(err)) throw err;
      }
      return;
    }
    const [, backupId, mode] = params;
    if (action !== 'restore' || !backupId || !RESTORE_MODES.includes(mode)) {
      return void await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    }
    if (Date.now() - Number(interaction.message?.createdTimestamp ?? 0) > BACKUP.CONFIRM_TTL_MS) {
      try {
        await interaction.update({ embeds: [warnEmbed('Confirmation Expired', 'This confirmation is older than 5 minutes. Run `/backup restore` again.')], components: [] });
      } catch (err) {
        if (!isInteractionGone(err)) throw err;
      }
      return;
    }
    try {
      await interaction.deferUpdate();
    } catch (err) {
      if (isInteractionGone(err)) return;
      throw err;
    }

    const row = await getBackup(guild.id, backupId, { withData: true });
    if (!row) return void await edit(interaction, errorEmbed('Backup Not Found', 'That backup no longer exists.'));
    let snapshot;
    try {
      snapshot = parseSnapshot(row.data);
    } catch (err) {
      return void await edit(interaction, errorEmbed('Backup Unreadable', err instanceof SnapshotError ? err.message : 'The backup could not be read.'));
    }
    const missing = missingRestorePermissions(guild.members?.me);
    if (missing.length) return void await edit(interaction, missingPermissionsEmbed(missing));
    const res = await startRestore({ guild, backupId, snapshot, mode, startedById: interaction.user.id, channelId: interaction.channelId });
    if (!res.ok) {
      return void await edit(interaction, warnEmbed('Restore Already Running', 'A restore is already running on this server. Check `/backup status`.'));
    }
    logger.info(`backups: restore of ${backupId} (${mode}) started in guild ${guild.id} by ${interaction.user.id}`);
    await edit(interaction, successEmbed('Restore Started',
      `Restoring in the background (${mode === 'full' ? 'full' : 'missing only'}). I post the result in <#${interaction.channelId}> when it is done; \`/backup status\` shows the progress. Nothing is deleted.`));
  } catch (err) {
    logger.error('backups: button failed', err);
    await safeReply(interaction, { embeds: [errorEmbed('Something Went Wrong', 'The restore could not be started. Please try again in a moment.')], ephemeral: true });
  }
}
