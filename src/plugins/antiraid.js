// Plugin `antiraid`: counts joins, starts a lockdown when a raid is detected, kicks or bans what joins during it and
// the young accounts, and ends it again (End button, `antiraid.unlock` job, /antiraid). State and effects:
// src/services/antiRaidService.js.
//
// Known limit: the welcome and captcha hooks still run for a member this plugin removes (nothing may call ctx.stop()
// but automod and custom commands). ctx.state.antiraid tells later hooks that the member was removed.
import { PERMISSION_LEVELS } from '../config/constants.js';
import { parseCustomId } from '../config/customIds.js';
import { PermanentJobError } from '../services/scheduler.js';
import { getSettingsSafe, peekSettings, updateSettings } from '../services/settingsService.js';
import {
  announceEnd, announceStart, detectionOf, endLockdown, harsher, isLocked, markActive, peekLocked, recordJoin,
  removeMember, reportYoung, startLockdown, tallyOf, youngAccount,
} from '../services/antiRaidService.js';
import { ackBudgetMs, withTimeout } from '../utils/async.js';
import { errorEmbed, infoEmbed, successEmbed, warnEmbed } from '../utils/embedBuilder.js';
import { isInteractionGone, safeReply } from '../utils/interactionHelpers.js';
import { resolvePermissionLevel } from '../utils/permissionChecker.js';
import logger from '../utils/logger.js';

const MINUTE_MS = 60_000;

/** When the member joined, never later than now. */
function joinTime(member, now) {
  const t = Number(member?.joinedTimestamp);
  return Number.isFinite(t) ? Math.min(t, now) : now;
}

/** Starts the lockdown of a detected raid and posts its alert. Never throws. */
async function startAndAnnounce(guild, settings, trigger, deps) {
  const res = await startLockdown({ guild, settings, trigger }, deps);
  if (!res.ok || !res.started) return res;
  await announceStart(guild, settings, res, trigger);
  return res;
}

/**
 * One member joined. Counts the join; a raid (antiRaidJoinThreshold joins within antiRaidJoinSeconds) starts a lockdown
 * in the background. While a lockdown is on every joiner gets antiRaidAction, and an account younger than
 * antiRaidMinAccountAgeDays gets antiRaidYoungAction; the harsher of the two is applied once. Bots are never touched.
 * Never throws.
 * @param {object} ctx HookContext with `guild`, `member`
 * @param {{ executeModAction?: Function, scheduleJob?: Function, cancelJobs?: Function, ensureGuildSettings?: Function }} [deps]
 * @returns {Promise<{ raid: boolean, start: Promise<object>|null, action: string|null, result: object|null }|null>}
 *   `start` is the background lockdown start (tests await it)
 */
export async function handleMemberAdd(ctx, deps = {}) {
  try {
    const { guild, member } = ctx;
    if (!guild || !member?.user || member.user.bot) return null;
    const settings = await ctx.getSettings();
    if (!settings) return null;
    const now = Date.now();
    const out = { raid: false, start: null, action: null, result: null };

    let locked = await isLocked(guild, now, deps);
    let trigger = null;
    // From here to markActive nothing awaits, so a burst of joins cannot start two lockdowns.
    if (!locked) locked = peekLocked(guild.id, now) === true;
    if (!locked) {
      const d = detectionOf(settings);
      const win = recordJoin(guild.id, joinTime(member, now), now, d.threshold, d.seconds);
      if (win.tripped) {
        locked = true;
        out.raid = true;
        trigger = { joins: win.count, seconds: d.seconds };
        markActive(guild.id, now + d.minutes * MINUTE_MS);
        out.start = startAndAnnounce(guild, settings, trigger, deps)
          .catch((err) => { logger.error(`[antiraid] ${guild.id}: lockdown start failed`, err); return { ok: false }; });
      }
    }

    const raidAction = locked ? settings.antiRaidAction : null;
    const young = youngAccount(member.user, settings, now);
    const action = harsher(raidAction, young ? settings.antiRaidYoungAction : null);
    if (!action) return out;
    out.action = action;

    const reasons = [];
    const details = [];
    if (locked && raidAction === action) {
      reasons.push('raid lockdown');
      details.push(trigger ? `Raid: ${trigger.joins} joins in ${trigger.seconds} seconds` : 'Joined during a raid lockdown');
    }
    if (young && (!reasons.length || settings.antiRaidYoungAction === action)) {
      reasons.push(`account is ${young.ageDays} day${young.ageDays === 1 ? '' : 's'} old (minimum ${young.minDays})`);
      details.push(`Account age: ${young.ageDays} day${young.ageDays === 1 ? '' : 's'} (minimum ${young.minDays})`);
    }

    const tally = locked ? tallyOf(guild.id) : null;
    if (action !== 'ALERT') {
      out.result = await removeMember({ guild, member, action, reason: `Anti-Raid: ${reasons.join('; ')}`, detail: details.join('\n') }, deps);
      if (out.result.ok) {
        if (ctx.state) ctx.state.antiraid = { action };
        if (tally) tally[action === 'BAN' ? 'banned' : 'kicked']++;
      } else if (!out.result.skipped && tally) {
        tally.failed++;
      }
    } else if (tally) {
      tally.flagged++;
    }

    if (young) {
      const done = out.result?.ok ? action : 'ALERT';
      await reportYoung(guild, settings, { member, ageDays: young.ageDays, minDays: young.minDays, action: done });
    }
    return out;
  } catch (err) {
    logger.error(`[antiraid] ${ctx?.guildId}: join handling failed`, err);
    return null;
  }
}

/** The alert channel was deleted: forget it (alerts fall back to the moderation log channel). */
export async function onChannelDelete(ctx) {
  const settings = await ctx.getSettings();
  if (!settings || settings.antiRaidAlertChannelId !== ctx.channel?.id) return;
  await updateSettings(ctx.guildId, { antiRaidAlertChannelId: null });
}

/** Sync hook filter: false when the memoised settings name another channel. */
export function mayBeAlertChannel(ctx) {
  const settings = peekSettings(ctx.guildId);
  if (!settings) return true;
  return Boolean(settings.antiRaidAlertChannelId) && settings.antiRaidAlertChannelId === ctx.channel?.id;
}

/**
 * Scheduler handler for `antiraid.unlock`. Idempotent; a lockdown that is not due yet (it was started again) is left
 * alone. Not module-gated: a lockdown always gets lifted. Throws only to be retried (database failure, or @everyone
 * could not be restored right now); a missing permission is permanent.
 */
export async function handleUnlockJob(job, ctx, deps = {}) {
  const guild = ctx?.guild;
  if (!guild) return;
  const settings = await getSettingsSafe(guild.id);
  const res = await endLockdown({ guild, settings, onlyIfDue: true }, deps);
  // A restore that can never succeed on its own must be said out loud, also on a retry that found the lockdown already over.
  if (res.wasActive || (res.pendingRestore && res.permanent)) await announceEnd(guild, settings, res, { how: 'expired' });
  if (res.pendingRestore) {
    const message = `antiraid.unlock: ${res.effects.errors.join(' ')}`;
    throw res.permanent ? new PermanentJobError(message) : new Error(message);
  }
}

const buttonSays = (interaction, embed) => safeReply(interaction, { embeds: [embed], ephemeral: true });

/**
 * Persistent `antiraid:end` button on the alert. Not module-gated (a lockdown can be ended while the module is off);
 * MODERATOR or higher is checked here. Never throws.
 */
export async function handleEndButton(interaction, client, deps = {}) {
  try {
    const { params: [action] } = parseCustomId(interaction.customId);
    if (action !== 'end') return void await safeReply(interaction, { content: 'This button is no longer active.', ephemeral: true });
    if (!interaction.inCachedGuild()) {
      return void await buttonSays(interaction, errorEmbed('Try Again', "I can't check your roles right now. Please try again in a moment."));
    }
    const { guild, guildId } = interaction;
    let settings = peekSettings(guildId);
    if (!settings) {
      settings = await withTimeout(getSettingsSafe(guildId), Math.min(1000, ackBudgetMs(interaction)), 'antiraid settings').catch(() => null);
    }
    if (resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.MODERATOR) {
      return void await buttonSays(interaction, errorEmbed('Moderators Only', 'Only moderators can end a raid lockdown.'));
    }

    try {
      await interaction.deferUpdate();
    } catch (err) {
      if (isInteractionGone(err)) return;
      throw err;
    }
    let res;
    try {
      res = await endLockdown({ guild, settings }, deps);
    } catch (err) {
      logger.error(`[antiraid] ${guildId}: ending the lockdown failed`, err);
      return void await buttonSays(interaction, errorEmbed('Could Not End Lockdown', 'The lockdown could not be ended. Please try again in a moment.'));
    }
    await interaction.editReply({ components: [] }).catch((err) => logger.debug(`[antiraid] end button: ${err?.message}`));
    if (!res.wasActive && !res.restored && !res.pendingRestore) {
      return void await buttonSays(interaction, infoEmbed('No Lockdown', 'There is no lockdown to end.'));
    }
    logger.info(`[antiraid] ${guildId}: lockdown ended with the button by ${interaction.user.id}`);
    await buttonSays(interaction, res.pendingRestore
      ? warnEmbed('Lockdown Ended', res.effects.errors.join('\n'))
      : successEmbed('Lockdown Ended', 'The lockdown is over.'));
    await announceEnd(guild, settings, res, { how: 'manual', endedById: interaction.user.id });
  } catch (err) {
    logger.error('[antiraid] end button failed', err);
  }
}

export const plugin = {
  id: 'antiraid',
  module: 'antiraid',
  hooks: [
    {
      event: 'guildMemberAdd',
      id: 'antiraid', // HOOK_ORDER.guildMemberAdd: after moderation, before logging / captcha / welcome
      filter: (ctx) => !ctx.member?.user?.bot,
      run: (ctx) => handleMemberAdd(ctx),
      timeoutMs: 15_000,
    },
    {
      event: 'channelDelete',
      id: 'antiraid',
      always: true,
      filter: mayBeAlertChannel,
      run: onChannelDelete,
    },
  ],
  components: [
    // module: null: ending a lockdown works while the module is off (the handler checks MODERATOR itself).
    { type: 'button', prefix: 'antiraid', module: null, handler: (interaction, client) => handleEndButton(interaction, client) },
  ],
  jobs: [
    { type: 'antiraid.unlock', handler: (job, ctx) => handleUnlockJob(job, ctx) },
  ],
  provides: {},
};
