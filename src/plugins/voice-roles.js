// Plugin `voice-roles`: members have a role while they are in a linked voice channel (or a voice channel of a linked category),
// and the "any voice channel" roles while they are in voice at all. Each voice change recomputes the member's roles from their
// current voice state and changes only what differs; guildReady and `/voicerole sync` reconcile everyone. Rules and settings
// edits: src/services/voiceRoleService.js; admin command: /voicerole. While the module is off nothing changes any role: the
// roles stay where they are, and `/voicerole sync` fixes them after it is turned back on.
import { isModuleEnabled } from '../config/modules.js';
import { peekSettings, getSettingsSafe, updateSettingsWith } from '../services/settingsService.js';
import {
  applyMember, reconcileGuild, voiceOf, hasVoiceRoles, patchDropChannel, patchDropRole, isReconciling, linksOf,
  releaseDroppedRoles,
} from '../services/voiceRoleService.js';
import { createSemaphore } from '../utils/async.js';
import logger from '../utils/logger.js';

// ── Filters (sync, cheap) ─────────────────────────────────────────────────────

/**
 * voiceStateUpdate: only a join, leave or move matters (mute, deafen, stream and camera changes cost nothing), and bots never
 * get voice roles. When the memoised settings show the module is off or nothing is set up the hook is skipped too; with no
 * memo (it lasts 30 s) the hook runs and decides.
 */
export function voiceFilter(ctx) {
  if (ctx.change === 'state') return false;
  if (ctx.member?.user?.bot === true) return false;
  const settings = peekSettings(ctx.guildId);
  if (!settings) return true;
  return isModuleEnabled(settings, 'voiceroles') && hasVoiceRoles(settings);
}

/** channelDelete: skipped once the memoised settings show no link uses the channel. */
export function channelDeleteFilter(ctx) {
  const settings = peekSettings(ctx.guildId);
  const id = ctx.channel?.id;
  if (!id) return false;
  return !settings || (Array.isArray(settings.voiceRoleLinks) && settings.voiceRoleLinks.some((l) => l?.channelId === id));
}

/** roleDelete: skipped once the memoised settings show the role is in no link and not in the any-list. */
export function roleDeleteFilter(ctx) {
  const settings = peekSettings(ctx.guildId);
  const id = ctx.role?.id;
  if (!id) return false;
  if (!settings) return true;
  return (Array.isArray(settings.voiceRoleLinks) && settings.voiceRoleLinks.some((l) => l?.roleId === id))
    || (Array.isArray(settings.voiceRoleAnyRoleIds) && settings.voiceRoleAnyRoleIds.includes(id));
}

// ── Voice changes ─────────────────────────────────────────────────────────────

/** voiceStateUpdate: recomputes the member's voice roles from their new voice state. Never throws. */
export async function onVoiceStateUpdate(ctx) {
  const settings = await ctx.getSettings();
  if (!settings || !hasVoiceRoles(settings)) return;
  await applyMember(ctx.guild, ctx.member, settings, { voice: voiceOf(ctx.newState, ctx.guild) });
}

// ── Startup reconcile ─────────────────────────────────────────────────────────

const startupQueue = createSemaphore(1);
const startupQueued = new Set();

/**
 * guildReady: reconciles the guild (members in voice get their roles, cached holders who are not in voice lose them). Runs
 * detached, one guild at a time, so the role calls never hold up the later guildReady hooks or the next guild's startup.
 */
export async function onGuildReady(ctx) {
  const settings = await ctx.getSettings();
  if (!settings || !hasVoiceRoles(settings)) return;
  const { guild } = ctx;
  if (startupQueued.has(guild.id) || isReconciling(guild.id)) return;
  startupQueued.add(guild.id);
  void startupQueue.run(async () => {
    startupQueued.delete(guild.id);
    // The queue can make this wait behind other guilds: look at the settings again, so a module turned off meanwhile is left alone.
    const fresh = await getSettingsSafe(guild.id);
    if (!fresh || !isModuleEnabled(fresh, 'voiceroles') || !hasVoiceRoles(fresh)) return;
    const res = await reconcileGuild(guild, fresh);
    if (res.changed || res.failed) {
      logger.info(`[voice-roles] ${guild.id}: startup sync checked ${res.checked}, gave ${res.added} and took ${res.removed} role(s), ${res.failed} failed`);
    }
  }).catch((err) => logger.error(`[voice-roles] startup sync failed for ${guild.id}`, err));
}

/** Test-only: resolves once every startup sync queued so far has finished. */
export function __startupSyncsIdle() {
  return startupQueue.run(() => {});
}

// ── Cleanup (runs while the module is off, too) ───────────────────────────────

// Role releases after a deleted channel run detached (many role calls must not hold up the other channelDelete hooks).
const releases = new Set();

/** Test-only: resolves once every role release started so far has finished. */
export async function __releasesIdle() {
  while (releases.size) await Promise.all([...releases]);
}

/**
 * channelDelete: drops the links of the deleted channel (or category). While the module is on, the roles of those links are
 * then taken from the members who hold them and are not entitled to them by another link or the any-list (otherwise the
 * role, no longer managed, would stay with them for good: members of a deleted category's channels, or of a deleted channel).
 */
export async function onChannelDelete(ctx) {
  const channelId = ctx.channel?.id;
  if (!channelId) return;
  let droppedRoleIds = [];
  const next = await updateSettingsWith(ctx.guildId, (cur) => {
    const patch = patchDropChannel(cur, channelId);
    droppedRoleIds = patch ? linksOf(cur).filter((l) => l.channelId === channelId).map((l) => l.roleId) : [];
    return patch;
  });
  const { guild } = ctx;
  if (!guild || !droppedRoleIds.length || !next || !isModuleEnabled(next, 'voiceroles')) return;
  const run = releaseDroppedRoles(guild, next, droppedRoleIds)
    .then((res) => {
      if (res.removed || res.failed) {
        logger.info(`[voice-roles] ${guild.id}: ${channelId} was deleted, took its role(s) from ${res.removed} member(s), ${res.failed} failed`);
      }
    })
    .catch((err) => logger.error(`[voice-roles] ${guild.id}: role release after deleting ${channelId} failed`, err))
    .finally(() => releases.delete(run));
  releases.add(run);
}

/** roleDelete: drops the deleted role from the links and the any-list. */
export async function onRoleDelete(ctx) {
  const roleId = ctx.role?.id;
  if (!roleId) return;
  await updateSettingsWith(ctx.guildId, (cur) => patchDropRole(cur, roleId));
}

export const plugin = {
  id: 'voice-roles',
  module: 'voiceroles',
  hooks: [
    // A fast hop can queue several role calls behind each other, so the soft timeout is generous (the work continues anyway).
    { event: 'voiceStateUpdate', id: 'voice-roles', filter: voiceFilter, run: onVoiceStateUpdate, timeoutMs: 30_000 },
    { event: 'guildReady', id: 'voice-roles', run: onGuildReady },
    // Cleanup runs even while the module is off, so a deleted channel or role never lingers in the settings.
    { event: 'channelDelete', id: 'voice-roles', always: true, filter: channelDeleteFilter, run: onChannelDelete },
    { event: 'roleDelete', id: 'voice-roles', always: true, filter: roleDeleteFilter, run: onRoleDelete },
  ],
  components: [],
  jobs: [],
  provides: {},
};
