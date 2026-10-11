import { SlashCommandBuilder, PermissionFlagsBits } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, errorEmbed, warnEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { parseDuration, formatDurationLong } from '../../utils/parseDuration.js';
import { replyError, requireBotPermissions, safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import { markBotDeletion } from '../../utils/deletionTracker.js';
import { formatNumber } from '../../utils/text.js';
import logger from '../../utils/logger.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
// gated: default true; allowInDMs: default false (guild-only)

// ── Constants ──────────────────────────────────────────────────────────────────

export const PAGE_SIZE = 100;                         // Discord max per fetch and per bulk delete
export const MAX_AMOUNT = 1000;                       // option max AND the limit when `amount` is omitted
export const SCAN_CAP = 1000;                         // hard cap on messages examined per run, in EVERY mode (R097)
export const AGE_LIMIT_MS = 14 * 24 * 60 * 60 * 1000;
export const AGE_MARGIN_MS = 5 * 60 * 1000;           // skip messages within 5 min of the 14-day limit
export const DEADLINE_MS = 10 * 60 * 1000;            // no new REST call after 10 min (the token lives 15)
const PROGRESS_AFTER_MS = 5_000;
const PROGRESS_INTERVAL_MS = 3_000;
export const FILTER_LABELS = Object.freeze({
  bots: 'Bots only',
  humans: 'Humans only',
  embeds: 'Has embed',
  attachments: 'Has attachment',
});

// Channel ids with a purge in progress on this shard. A guild's interactions always land on its
// own shard, so no Redis lock is needed (and nothing degrades while Redis is down).
const ACTIVE_PURGES = new Set();

// ── Command definition ─────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('purge')
  .setDescription('Bulk-delete recent messages in this channel (MEE6 /clear)')
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addIntegerOption((o) => o.setName('amount').setDescription('How many messages to delete (1-1000)').setMinValue(1).setMaxValue(1000))
  .addStringOption((o) => o.setName('timeframe').setDescription('Only messages from the last X, e.g. 30m, 2h, 7d (max 14d)').setMaxLength(32))
  .addUserOption((o) => o.setName('user').setDescription('Only messages from this user'))
  .addStringOption((o) => o.setName('filter').setDescription('Only messages of this kind').addChoices(
    { name: 'Bots only', value: 'bots' }, { name: 'Humans only', value: 'humans' },
    { name: 'Has embed', value: 'embeds' }, { name: 'Has attachment', value: 'attachments' }))
  .addStringOption((o) => o.setName('contains').setDescription('Only messages containing this text').setMaxLength(100))
  .addBooleanOption((o) => o.setName('pinned').setDescription('Also delete pinned messages (default: keep them)'));

// ── Helpers ────────────────────────────────────────────────────────────────────

const PERMISSION_CODES = new Set([50001, 50013]); // Missing Access, Missing Permissions
const CHANNEL_GONE_CODE = 10003;                  // Unknown Channel
const MESSAGE_GONE_CODE = 10008;                  // Unknown Message

function stopForError(err) {
  const code = err?.code;
  if (PERMISSION_CODES.has(code)) return 'permission';
  if (code === CHANNEL_GONE_CODE) return 'channel-gone';
  return null;
}

/**
 * Pure. Every condition is ANDed. `contains` must already be trimmed and lowercased (or null).
 * R247: 'embeds' matches messages that have an embed, attachments no longer count.
 * @param {object} message
 * @param {{ userId?: string|null, filter?: string|null, contains?: string|null }} [opts]
 * @returns {boolean}
 */
export function matchesFilters(message, { userId = null, filter = null, contains = null } = {}) {
  if (userId && message.author?.id !== userId) return false;
  if (filter === 'bots' && message.author?.bot !== true && !message.webhookId) return false; // webhooks count as bots
  if (filter === 'humans' && (message.author?.bot === true || message.webhookId)) return false;
  if (filter === 'embeds' && !(message.embeds?.length > 0)) return false;
  if (filter === 'attachments' && !(message.attachments?.size > 0)) return false;
  if (contains && !(message.content ?? '').toLowerCase().includes(contains)) return false;
  return true;
}

function smallestId(messages) {
  let best = messages[0].id;
  for (const m of messages) {
    try {
      if (BigInt(m.id) < BigInt(best)) best = m.id;
    } catch {
      // a non-numeric id never happens on Discord; keep the previous best
    }
  }
  return best;
}

/**
 * Pages the channel newest to oldest and collects the messages to delete (R097, R098).
 * Filters and the limit are applied inside the loop; every mode is bounded by `scanCap`
 * examined messages and by `deadline`. Never throws.
 * @param {import('discord.js').TextBasedChannel} channel
 * @param {object} opts
 * @returns {Promise<{ targets: object[], scanned: number, skippedPinned: number, skippedUndeletable: number,
 *   stopReason: string, errorCode: number|null }>}
 */
export async function collectPurgeTargets(channel, {
  limit = MAX_AMOUNT, cutoffTs = -Infinity, cutoffKind = 'age',
  userId = null, filter = null, contains = null, includePinned = false,
  scanCap = SCAN_CAP, deadline = Infinity,
} = {}) {
  const targets = [];
  let scanned = 0;
  let skippedPinned = 0;
  let skippedUndeletable = 0;
  let stopReason = 'exhausted';
  let errorCode = null;
  let before;

  try {
    for (;;) {
      if (Date.now() > deadline) { stopReason = 'deadline'; break; }
      const pageSize = Math.min(PAGE_SIZE, scanCap - scanned);
      if (pageSize <= 0) { stopReason = 'scan-cap'; break; }

      let page;
      try {
        // cache:false keeps up to 1,000 messages out of the channel cache.
        page = await channel.messages.fetch({ limit: pageSize, before, cache: false });
      } catch (err) {
        stopReason = stopForError(err) ?? 'error';
        errorCode = err?.code ?? null;
        break;
      }

      const msgs = [...(page?.values?.() ?? [])];
      if (msgs.length === 0) { stopReason = 'exhausted'; break; }
      msgs.sort((a, b) => b.createdTimestamp - a.createdTimestamp);

      let done = false;
      for (const m of msgs) {
        // Pages come newest first, so everything after the first message at or below the cut-off is older too.
        if (m.createdTimestamp <= cutoffTs) { stopReason = cutoffKind === 'timeframe' ? 'timeframe' : 'age'; done = true; break; }
        scanned++;
        if (!matchesFilters(m, { userId, filter, contains })) continue;
        if (m.pinned && !includePinned) { skippedPinned++; continue; }
        if (!m.deletable) { skippedUndeletable++; continue; }
        targets.push(m);
        if (targets.length >= limit) { stopReason = 'limit'; done = true; break; }
      }
      if (done) break;
      if (msgs.length < pageSize) { stopReason = 'exhausted'; break; }
      before = smallestId(msgs);
    }
  } catch (err) {
    // Defensive: nothing above should throw, but a purge must always produce a report.
    logger.error('purge: collect failed', err);
    stopReason = 'error';
    errorCode = err?.code ?? null;
  }

  return { targets, scanned, skippedPinned, skippedUndeletable, stopReason, errorCode };
}

/**
 * Deletes the messages in chunks of 100, newest first. A message is counted as deleted only when its
 * REST call resolved (R096). markBotDeletion runs right before every REST delete so the log hooks can
 * label the deletions. Never throws.
 * @param {import('discord.js').TextBasedChannel} channel
 * @param {Array<{ id: string }>} messages
 * @param {{ reason?: string, deadline?: number, onProgress?: ((deleted: number) => void)|null }} [opts]
 * @returns {Promise<{ deleted: number, alreadyGone: number, failed: number,
 *   stopReason: null|'permission'|'channel-gone'|'deadline', errorCode: number|null }>}
 */
export async function deleteInChunks(channel, messages, { reason = 'purge', deadline = Infinity, onProgress = null } = {}) {
  let deleted = 0;
  let alreadyGone = 0;
  let failed = 0;
  let stopReason = null;
  let errorCode = null;
  let warned = false;

  async function single(id) {
    try {
      await channel.messages.delete(id);
      deleted++;
    } catch (err) {
      const code = err?.code;
      if (code === MESSAGE_GONE_CODE) { alreadyGone++; return; }
      const stop = stopForError(err);
      if (stop) { stopReason = stop; errorCode = code; return; }
      failed++;
      if (!warned) {
        warned = true;
        logger.warn(`purge: delete ${id} failed (${code ?? err?.message})`);
      }
    }
  }

  try {
    outer: for (let i = 0; i < messages.length; i += PAGE_SIZE) {
      if (Date.now() > deadline) { stopReason = 'deadline'; break; }
      const ids = messages.slice(i, i + PAGE_SIZE).map((m) => m.id);

      markBotDeletion(ids, reason); // immediately before the REST call (TTL 60 s per call)
      if (ids.length === 1) {
        await single(ids[0]);
        if (stopReason) break;
      } else {
        try {
          await channel.bulkDelete(ids, false);
          deleted += ids.length;
        } catch (err) {
          const stop = stopForError(err);
          if (stop) { stopReason = stop; errorCode = err?.code ?? null; break; }
          // 50034 (too old), 50035, 10008, 5xx ...: delete this chunk one message at a time.
          for (const id of ids) {
            if (Date.now() > deadline) { stopReason = 'deadline'; break outer; }
            markBotDeletion(id, reason); // refresh right before each single delete
            await single(id);
            if (stopReason) break outer;
          }
        }
      }

      try {
        onProgress?.(deleted);
      } catch (err) {
        logger.debug(`purge: progress callback failed: ${err?.message}`);
      }
    }
  } catch (err) {
    // Defensive: nothing above should throw, but a purge must always produce a report.
    logger.error('purge: delete failed', err);
  }

  return { deleted, alreadyGone, failed, stopReason, errorCode };
}

/**
 * Pure: builds the final (ephemeral) report embed.
 * @param {object} result - `{ ...collected, deleted, alreadyGone, failed, deleteStop, opts }`
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildPurgeReport(result) {
  const {
    targets = [], scanned = 0, skippedPinned = 0, skippedUndeletable = 0,
    stopReason = null, errorCode = null,
    deleted = 0, alreadyGone = 0, failed = 0, deleteStop = null, opts = {},
  } = result ?? {};
  const targetCount = Array.isArray(targets) ? targets.length : Number(targets) || 0;
  const { amount = null, timeframeMs = null, userId = null, filter = null, contains = null } = opts ?? {};

  const lines = [`**Deleted:** ${formatNumber(deleted)} message(s)`, `**Checked:** ${formatNumber(scanned)} message(s)`];
  if (amount) lines.push(`**Limit:** ${amount}`);
  if (timeframeMs) lines.push(`**Timeframe:** last ${formatDurationLong(timeframeMs)}`);
  if (userId) lines.push(`**User:** <@${userId}>`);
  if (filter && FILTER_LABELS[filter]) lines.push(`**Filter:** ${FILTER_LABELS[filter]}`);
  if (contains) lines.push(`**Contains:** \`${String(contains).replace(/`/g, '')}\``);

  const stops = new Set([stopReason, deleteStop]);
  const notes = [];
  if (stopReason === 'age') notes.push('Stopped at messages older than 14 days; Discord does not let bots bulk-delete those.');
  if (stopReason === 'scan-cap') {
    notes.push(`Stopped after checking ${formatNumber(SCAN_CAP)} messages (the per-run limit). Run the command again to continue.`);
  }
  if (skippedPinned > 0) notes.push(`Kept ${skippedPinned} pinned message(s). Set \`pinned\` to True to include them.`);
  if (skippedUndeletable > 0) notes.push(`Skipped ${skippedUndeletable} system message(s) that cannot be deleted.`);
  if (alreadyGone > 0) notes.push(`${alreadyGone} message(s) were already deleted by someone else.`);
  if (failed > 0) notes.push(`${failed} message(s) could not be deleted.`);
  if (stops.has('permission')) notes.push('Stopped early: I lost access or the Manage Messages permission in this channel.');
  if (stops.has('channel-gone')) notes.push('Stopped early: this channel was deleted.');
  if (stops.has('deadline')) notes.push('Stopped early: the purge reached the 10-minute time limit.');
  if (stopReason === 'error') {
    notes.push(`Stopped early: Discord returned an error${errorCode ? ` (code ${errorCode})` : ''}. Try again later.`);
  }
  const noteText = notes.map((n) => `• ${n}`).join('\n');
  const withNotes = (head) => (noteText ? `${head}\n\n${noteText}` : head);

  const early = ['permission', 'channel-gone', 'deadline', 'error'].some((s) => stops.has(s));

  if (deleted > 0) {
    const body = withNotes(lines.join('\n'));
    return early || failed > 0 ? warnEmbed('Purge Partly Complete', body) : successEmbed('Purge Complete', body);
  }

  if (targetCount === 0) {
    if (['permission', 'channel-gone', 'deadline', 'error'].includes(stopReason)) {
      if (stopReason === 'permission' && scanned === 0) {
        return errorEmbed('Purge Failed', "I can't read the message history in this channel.");
      }
      return errorEmbed('Purge Failed', noteText);
    }
    let first;
    if (scanned === 0 && stopReason === 'exhausted') first = 'This channel has no messages.';
    else if (scanned === 0 && stopReason === 'age') first = 'Every message here is older than 14 days. Discord does not let bots bulk-delete those.';
    else if (scanned === 0 && stopReason === 'timeframe') first = `No messages were sent in the last ${formatDurationLong(timeframeMs)}.`;
    else first = `No messages matched your options among the ${formatNumber(scanned)} message(s) I checked.`;
    return errorEmbed('Nothing to Delete', withNotes(first));
  }

  return errorEmbed('Purge Failed', withNotes(lines.join('\n')));
}

// ── Command ────────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  // 1. Channel.
  const channel = interaction.channel;
  if (!channel || typeof channel.bulkDelete !== 'function') {
    return void await replyError(interaction, 'Unsupported Channel', 'Use this command in a text channel, a thread, or a voice/stage channel chat.');
  }

  // 2. Options.
  const amount = interaction.options.getInteger('amount');
  const timeframeStr = interaction.options.getString('timeframe')?.trim() || null;
  const user = interaction.options.getUser('user');
  const filter = interaction.options.getString('filter');
  const contains = interaction.options.getString('contains')?.trim().toLowerCase() || null;
  const includePinned = interaction.options.getBoolean('pinned') ?? false;

  if (!amount && !timeframeStr) {
    return void await replyError(interaction, 'Missing Options', 'Provide an `amount`, a `timeframe`, or both.');
  }
  let timeframeMs = null;
  if (timeframeStr) {
    timeframeMs = parseDuration(timeframeStr);
    if (timeframeMs === null) {
      const clean = timeframeStr.replace(/`/g, '');
      return void await replyError(interaction, 'Invalid Timeframe', `Could not read \`${clean}\`. Try \`30m\`, \`2h\` or \`7d\`.`);
    }
    if (timeframeMs > AGE_LIMIT_MS) {
      return void await replyError(interaction, 'Timeframe Too Large', 'Discord only lets bots bulk-delete messages from the last **14 days**.');
    }
  }

  // 3. Archived thread.
  if (channel.isThread?.() && channel.archived) {
    return void await replyError(interaction, 'Thread Archived', 'This thread is archived. Unarchive it, then try again.');
  }

  // 4. Bot timed out.
  const me = interaction.guild?.members?.me;
  if (me?.isCommunicationDisabled?.() && !channel.permissionsFor?.(me)?.has(PermissionFlagsBits.Administrator)) {
    return void await replyError(interaction, 'I Am Timed Out', 'I am timed out in this server, so I cannot delete messages until the timeout ends.');
  }

  // 5. Bot permissions (R096): checked before the deferral, so the refusal is immediate and private.
  if (!(await requireBotPermissions(interaction, ['ViewChannel', 'ReadMessageHistory', 'ManageMessages'], { channel }))) return;

  // 6. One purge per channel.
  if (ACTIVE_PURGES.has(channel.id)) {
    return void await replyError(interaction, 'Purge In Progress', 'Another purge is already running in this channel. Wait for it to finish, then try again.');
  }
  ACTIVE_PURGES.add(channel.id);
  try {
    // 7. Defer (D3: always private, whatever modVisibility says).
    if (!(await safeDefer(interaction, { ephemeral: true }))) return;

    let lastProgress = null;
    try {
      // 8. Cut-offs.
      const start = Date.now();
      const deadline = start + DEADLINE_MS;
      const ageCutoffTs = start - (AGE_LIMIT_MS - AGE_MARGIN_MS);
      const tfCutoffTs = timeframeMs ? start - timeframeMs : -Infinity;
      const cutoffTs = Math.max(ageCutoffTs, tfCutoffTs);
      const cutoffKind = tfCutoffTs > ageCutoffTs ? 'timeframe' : 'age';
      const limit = amount ?? MAX_AMOUNT;

      // 9. Collect.
      const collected = await collectPurgeTargets(channel, {
        limit, cutoffTs, cutoffKind, userId: user?.id ?? null, filter, contains, includePinned, deadline,
      });

      // 10. Delete.
      let deletion = { deleted: 0, alreadyGone: 0, failed: 0, stopReason: null, errorCode: null };
      if (collected.targets.length > 0) {
        const reason = `/purge by ${interaction.user.tag} (${interaction.user.id})`;
        let lastProgressAt = 0;
        const total = collected.targets.length;
        const onProgress = (deletedSoFar) => {
          const now = Date.now();
          if (now - start < PROGRESS_AFTER_MS || now - lastProgressAt < PROGRESS_INTERVAL_MS) return;
          lastProgressAt = now;
          // Not awaited: the deletion loop never waits for the edit.
          lastProgress = interaction.editReply({
            embeds: [infoEmbed('Purging…', `Deleted ${formatNumber(deletedSoFar)} of ${formatNumber(total)} message(s) so far…`)],
          }).catch(() => {});
        };
        deletion = await deleteInChunks(channel, collected.targets, { reason, deadline, onProgress });
      }

      // 11. Report. A late progress edit must never overwrite it, so wait for the last one first.
      await lastProgress;
      const report = buildPurgeReport({
        ...collected,
        deleted: deletion.deleted,
        alreadyGone: deletion.alreadyGone,
        failed: deletion.failed,
        deleteStop: deletion.stopReason,
        opts: { amount, timeframeMs, userId: user?.id ?? null, filter, contains, includePinned },
      });
      await safeReply(interaction, { embeds: [report] });
    } catch (err) {
      logger.error('purge failed', err);
      await lastProgress;
      await safeReply(interaction, {
        embeds: [errorEmbed('Purge Failed', 'Something went wrong while purging. Some messages may have been deleted.')],
      });
    }
  } finally {
    ACTIVE_PURGES.delete(channel.id);
  }
}
