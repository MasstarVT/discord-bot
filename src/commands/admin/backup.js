// /backup create | list | info | export | status | delete | restore. Admin command (ephemeral replies: a backup describes the
// whole server) that keeps working while the Server Backups module is off (gated = false), so a server can be backed up and
// the schedule set up first. DELETE and RESTORE are for the SERVER OWNER only (`interaction.guild.ownerId`; the bot owner's
// PERMISSION_LEVELS.OWNER does not count), checked first, before any await, and again in the confirmation button.
// The work is in src/services/backupService.js and src/utils/guildSnapshot.js; the restore never runs here, it is a job.
import { AttachmentBuilder, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { isModuleEnabled } from '../../config/modules.js';
import {
  BACKUP, RESTORE_MODES, backupLabel, buildInfoEmbed, buildListEmbed, buildPreviewEmbed, buildStatusEmbed, countBackups, createBackup,
  deleteBackup, ensureAutoJob, exportable, getBackup, getRestoreJob, listBackups, missingPermissionsEmbed, missingRestorePermissions,
  newestAutomatic, ownerOnlyEmbed, parseSnapshot, restoreButtons, restorePreview, searchBackups,
} from '../../services/backupService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed, successEmbed, warnEmbed } from '../../utils/embedBuilder.js';
import { SnapshotError } from '../../utils/guildSnapshot.js';
import { awaitConfirm, respondAutocomplete, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { formatNumber } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'backups';
export const gated = false;                          // config command: works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

const backupOption = (o) => o.setName('backup').setDescription('Which backup (type a name).').setRequired(true).setAutocomplete(true);

export const data = new SlashCommandBuilder()
  .setName('backup')
  .setDescription('Back up and restore roles, channels and permissions.')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('create').setDescription('Save the roles, channels and permissions now.')
    .addStringOption((o) => o.setName('name').setDescription('A name for it.').setMaxLength(BACKUP.NAME_MAX)))
  .addSubcommand((s) => s.setName('list').setDescription('Show the saved backups.'))
  .addSubcommand((s) => s.setName('info').setDescription('Show what a backup holds.').addStringOption(backupOption))
  .addSubcommand((s) => s.setName('export').setDescription('Download a backup as a JSON file.').addStringOption(backupOption))
  .addSubcommand((s) => s.setName('status').setDescription('Show the schedule and any restore in progress.'))
  .addSubcommand((s) => s.setName('delete').setDescription('Delete a backup. Server owner only.').addStringOption(backupOption))
  .addSubcommand((s) => s.setName('restore').setDescription('Rebuild roles and channels from a backup. Server owner only.')
    .addStringOption(backupOption)
    .addStringOption((o) => o.setName('mode').setDescription('What to rebuild (default: missing only).')
      .addChoices({ name: 'Missing only: recreate what is gone', value: 'missing' }, { name: 'Full: also reset what exists', value: 'full' })));

const MSG = {
  NOT_FOUND: () => errorEmbed('Backup Not Found', 'Pick a backup from the list that appears as you type in the **backup** option.'),
  FAILED: (what) => errorEmbed(`Could Not ${what}`, 'Something went wrong. Please try again in a moment.'),
  UNREADABLE: (err) => errorEmbed('Backup Unreadable', err instanceof SnapshotError ? err.message : 'The backup could not be read.'),
};

const isOwner = (interaction) => Boolean(interaction.guild) && interaction.user.id === interaction.guild.ownerId;
// Always private: a follow-up after a confirm dialog is a new message and would otherwise be public.
const finish = (interaction, payload) => safeReply(interaction, { allowedMentions: { parse: [] }, ephemeral: true, ...payload });
const embedOnly = (embed) => ({ embeds: [embed] });

/** A restore in progress for this backup (it must not be deleted under the job). */
async function restoringThis(guildId, backupId) {
  const job = await getRestoreJob(guildId);
  return Boolean(job) && (job.status === 'PENDING' || job.status === 'RUNNING') && job.payload?.backupId === backupId;
}

/** Keeps the recurring backup.auto job alive whenever an admin touches the module (a dashboard change alone cannot). */
function nudgeSchedule(guildId, settings) {
  if (settings && isModuleEnabled(settings, 'backups') && Number(settings.backupIntervalHours) > 0) ensureAutoJob(guildId).catch(() => {});
}

// ── /backup create ────────────────────────────────────────────────────────────

async function runCreate(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guild, guildId } = interaction;
  const res = await createBackup({ guild, createdById: interaction.user.id, name: interaction.options.getString('name') });
  if (!res.ok) return finish(interaction, embedOnly(errorEmbed('Could Not Make The Backup', res.error)));
  const settings = await getSettingsSafe(guildId);
  nudgeSchedule(guildId, settings);
  logger.info(`[backups] ${guildId}: backup ${res.backup.id} made by ${interaction.user.id}`);
  return finish(interaction, embedOnly(successEmbed('Backup Saved',
    `Saved **${formatNumber(res.backup.roleCount)}** roles and **${formatNumber(res.backup.channelCount)}** categories and channels with their permissions.\n`
    + 'Threads, messages, members and their roles, emojis, stickers, webhooks and bot settings are not part of a backup.',
    `ID ${res.backup.id}`)));
}

// ── /backup list | info | export ──────────────────────────────────────────────

async function runList(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const [settings, rows] = await Promise.all([getSettingsSafe(interaction.guildId), listBackups(interaction.guildId)]);
  return finish(interaction, embedOnly(buildListEmbed(settings, rows)));
}

async function loadSnapshot(interaction) {
  const row = await getBackup(interaction.guildId, interaction.options.getString('backup', true), { withData: true });
  if (!row) return { error: MSG.NOT_FOUND() };
  try {
    return { row, snapshot: parseSnapshot(row.data) };
  } catch (err) {
    return { error: MSG.UNREADABLE(err) };
  }
}

async function runInfo(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  const { row, snapshot, error } = await loadSnapshot(interaction);
  if (error) return finish(interaction, embedOnly(error));
  return finish(interaction, embedOnly(buildInfoEmbed(settings, row, snapshot)));
}

async function runExport(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { row, error } = await loadSnapshot(interaction);
  if (error) return finish(interaction, embedOnly(error));
  const day = row.createdAt.toISOString().slice(0, 10).replace(/-/g, '');
  const file = new AttachmentBuilder(Buffer.from(JSON.stringify(exportable(row.data), null, 2), 'utf8'), { name: `backup-${day}-${row.id.slice(-8)}.json` });
  return finish(interaction, {
    embeds: [successEmbed('Backup Exported', `**${backupLabel(row)}**. Only you can see this file. It lists role permissions and channel layout, so keep it private.`)],
    files: [file],
  });
}

// ── /backup status ────────────────────────────────────────────────────────────

async function runStatus(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guild, guildId } = interaction;
  const settings = await getSettingsSafe(guildId);
  const [counts, newest, restoreJob] = await Promise.all([countBackups(guildId), newestAutomatic(guildId), getRestoreJob(guildId)]);
  const problems = [];
  const me = guild.members?.me;
  if (!me?.permissions?.has(PermissionFlagsBits.ManageRoles)) problems.push('I need **Manage Roles** to restore roles and channel permissions.');
  if (!me?.permissions?.has(PermissionFlagsBits.ManageChannels)) problems.push('I need **Manage Channels** to restore categories and channels.');
  if (settings && !isModuleEnabled(settings, 'backups')) problems.push('The module is off: automatic backups are paused. `/module enable backups` turns them on. Manual backups and restores still work.');
  nudgeSchedule(guildId, settings);
  return finish(interaction, embedOnly(buildStatusEmbed({ settings, counts, newest, restoreJob, problems })));
}

// ── /backup delete (server owner) ─────────────────────────────────────────────

/** @param {object} [deps] test seam: { confirm } */
export async function runDelete(interaction, deps = {}) {
  if (!isOwner(interaction)) return safeReply(interaction, { ...embedOnly(ownerOnlyEmbed()), ephemeral: true });
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guildId } = interaction;
  const row = await getBackup(guildId, interaction.options.getString('backup', true));
  if (!row) return finish(interaction, embedOnly(MSG.NOT_FOUND()));
  if (await restoringThis(guildId, row.id)) {
    return finish(interaction, embedOnly(warnEmbed('Restore Running', 'This backup is being restored right now. Wait until the restore is finished.')));
  }
  const confirm = deps.confirm ?? awaitConfirm;
  const yes = await confirm(interaction, {
    title: 'Delete this backup?',
    description: `**${backupLabel(row)}** (${formatNumber(row.roleCount)} roles, ${formatNumber(row.channelCount)} channels) will be deleted for good.`,
    confirmLabel: 'Delete', danger: true, timeoutMs: 30_000,
  });
  if (!yes) return;
  // The restore may have started while the owner thought about it.
  if (await restoringThis(guildId, row.id)) {
    return finish(interaction, embedOnly(warnEmbed('Restore Running', 'A restore of this backup just started, so it was not deleted.')));
  }
  const count = await deleteBackup(guildId, row.id);
  logger.info(`[backups] ${guildId}: backup ${row.id} deleted by ${interaction.user.id}`);
  return finish(interaction, embedOnly(count ? successEmbed('Backup Deleted', `**${backupLabel(row)}** is gone.`) : MSG.NOT_FOUND()));
}

// ── /backup restore (server owner) ────────────────────────────────────────────

export async function runRestore(interaction) {
  if (!isOwner(interaction)) return safeReply(interaction, { ...embedOnly(ownerOnlyEmbed()), ephemeral: true });
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const { guild, guildId } = interaction;
  const mode = interaction.options.getString('mode') ?? 'missing';
  if (!RESTORE_MODES.includes(mode)) return finish(interaction, embedOnly(MSG.FAILED('Restore')));
  const { row, snapshot, error } = await loadSnapshot(interaction);
  if (error) return finish(interaction, embedOnly(error));
  const job = await getRestoreJob(guildId);
  if (job && (job.status === 'PENDING' || job.status === 'RUNNING')) {
    return finish(interaction, embedOnly(warnEmbed('Restore Already Running', 'A restore is already running on this server. Check `/backup status`.')));
  }
  const missing = missingRestorePermissions(guild.members?.me);
  if (missing.length) return finish(interaction, embedOnly(missingPermissionsEmbed(missing)));
  const preview = restorePreview(guild, snapshot, mode, { prevJob: job, backupId: row.id });
  return finish(interaction, { embeds: [buildPreviewEmbed(row, preview)], components: [restoreButtons(row.id, mode)] });
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

const RUN = { create: runCreate, list: runList, info: runInfo, export: runExport, status: runStatus, delete: runDelete, restore: runRestore };

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  try {
    return await RUN[sub]?.(interaction);
  } catch (err) {
    logger.error(`[backups] ${interaction.guildId}: /backup ${sub} failed`, err);
    return finish(interaction, embedOnly(MSG.FAILED(sub === 'restore' ? 'Restore' : 'Finish')));
  }
}

export async function autocomplete(interaction) {
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name !== 'backup') return respondAutocomplete(interaction, []);
    const rows = await searchBackups(interaction.guildId, focused.value);
    return respondAutocomplete(interaction, rows.map((r) => ({ name: backupLabel(r), value: r.id })));
  } catch (err) {
    logger.debug(`[backups] autocomplete failed: ${err?.message}`);
    return respondAutocomplete(interaction, []);
  }
}
