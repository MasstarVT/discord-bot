// Live Role plugin (pkg-liverole.md): a role held while a member's Discord presence shows a Streaming activity (Twitch or YouTube),
// MEE6's Twitch Alerts "live-streaming role" without the Twitch API. All logic lives here: the plugin declaration, the pure helpers,
// and the async engine (reconcileMember, syncGuild). src/commands/admin/liverole.js imports the named exports. Nothing runs at import:
// the only module-scope state is two keyed mutexes, two throttles, a waiting-set and frozen constants.
import { ActivityType, escapeMarkdown } from 'discord.js';
import { getSettingsSafe, peekSettings } from '../services/settingsService.js';
import { checkAssignableRole } from '../utils/interactionHelpers.js';
import { createKeyedMutex, createThrottle } from '../utils/async.js';
import { fetchAllMembers } from '../utils/guildMembers.js';
import { clampText, formatNumber } from '../utils/text.js';
import logger from '../utils/logger.js';

// ── Constants ─────────────────────────────────────────────────────────────────

export const LIVEROLE = Object.freeze({
  SYNC_MAX_CHANGES: 50,            // role edits attempted per sync (adds + checks + removes)
  SET_MAX_STRIP: 50,               // /liverole set refuses a role that would be stripped from more members than this
  FULL_FETCH_MAX_MEMBERS: 25_000,  // above this memberCount, syncs use the member cache only
  FULL_FETCH_TIMEOUT_MS: 60_000,
  FULL_FETCH_INTERVAL_MS: 10 * 60_000,
  COMMAND_SYNC_WAIT_MS: 25_000,    // how long a command waits for its sync before answering "still syncing"
  WARN_INTERVAL_MS: 60 * 60_000,
  STATUS_LIST_MAX: 15,             // holders listed by /liverole status
});

export const REASONS = Object.freeze({   // audit-log reasons (each ≤ 512 chars)
  presenceAdd: 'Live role: started streaming',
  presenceRemove: 'Live role: stopped streaming',
  syncAdd: 'Live role sync: member is streaming',
  syncRemove: 'Live role sync: member is not streaming or not eligible',
  clear: 'Live role turned off with /liverole disable',
});

// ── In-process state (per shard; lost on restart, which is fine: the startup sweep re-derives it) ──

/** Serialises every role decision for one member (presence hook and sync): `${guildId}:${userId}`. */
const memberLock = createKeyedMutex();
/** Serialises whole-guild syncs: `guildId`. Lock order is syncLock, then memberLock. */
const syncLock = createKeyedMutex();
/** At most one warning per guild and kind per hour. */
const warnThrottle = createThrottle(LIVEROLE.WARN_INTERVAL_MS);
/** At most one full member-list fetch per guild every 10 minutes. */
const fetchThrottle = createThrottle(LIVEROLE.FULL_FETCH_INTERVAL_MS);
/**
 * Flap guard: member keys that already have a presence-sourced reconcile queued behind the running one.
 * A further presence event for that member is dropped, because the queued run reads the newest presence
 * from the cache when it starts, so the final state still converges (at most one running and one waiting).
 */
const waiting = new Set();

const warn = (guildId, kind, text) => {
  if (warnThrottle(`${guildId}:${kind}`)) logger.warn(`[liverole] ${guildId}: ${text}`);
};

// ── Pure helpers ──────────────────────────────────────────────────────────────

/** True when the presence has any Streaming activity. null/undefined/malformed → false. */
export function isStreaming(presence) {
  return Array.isArray(presence?.activities) && presence.activities.some((a) => a?.type === ActivityType.Streaming);
}

/** True when the streaming state differs. A null oldPresence counts as "not streaming". */
export function streamingChanged(oldPresence, newPresence) {
  return isStreaming(oldPresence) !== isStreaming(newPresence);
}

/**
 * Sync, cheap hook filter. `peeked` = peekSettings(guildId) (memo-only; may be null).
 * Skip unless the streaming state changed; also skip when the memo PROVES the feature is off or has no role.
 * A null memo never skips (settings are then loaded by the runner).
 */
export function shouldHandlePresence(oldPresence, newPresence, peeked) {
  if (!streamingChanged(oldPresence, newPresence)) return false;
  if (peeked && (peeked.liveRoleEnabled !== true || !peeked.liveRoleId)) return false;
  return true;
}

/** Platform label of the Streaming activity ("Twitch", "YouTube"), markdown-escaped, ≤ 32 chars; null when not streaming. */
export function streamLabel(presence) {
  const a = Array.isArray(presence?.activities) ? presence.activities.find((x) => x?.type === ActivityType.Streaming) : null;
  return a ? clampText(escapeMarkdown(String(a.name || 'Streaming')), 32) : null;
}

/**
 * Does a member satisfy liveRoleRequiredRoleId?
 * memberRoleIds: anything with .has(id) (Set, Collection, member.roles.cache).
 * Fail closed when the required role was deleted.
 */
export function meetsRequirement({ requiredRoleId, requiredRoleExists = true, memberRoleIds, guildId }) {
  if (!requiredRoleId || requiredRoleId === guildId) return true;
  if (!requiredRoleExists) return false;
  return !!memberRoleIds?.has?.(requiredRoleId);
}

/** Desired change for one member. mode 'clear' removes from every holder. */
export function planMember({ mode = 'sync', streaming, qualifies, holds }) {
  if (mode === 'clear') return holds ? 'remove' : null;
  const want = !!streaming && !!qualifies;
  if (want && !holds) return 'add';
  if (!want && holds) return 'remove';
  return null;
}

const unique = (ids) => [...new Set(ids)];

/**
 * Candidate lists for a whole-guild sync. The final decision is always re-made on fresh state per member.
 * - holderIds: non-bot, non-partial members that hold the live role
 * - streamerIds: non-bot, cached, non-partial members whose presence is Streaming
 * - unknownIds: streaming users whose member is missing from the cache or partial (their roles are unknown)
 * - qualifies(id) → boolean
 * @returns {{ add: string[], remove: string[], check: string[] }} de-duplicated, input order kept
 */
export function planSync({ mode = 'sync', holderIds = [], streamerIds = [], unknownIds = [], qualifies = () => true } = {}) {
  if (mode === 'clear') return { add: [], remove: unique(holderIds), check: [] };
  const H = new Set(holderIds);
  const S = new Set(streamerIds);
  return {
    add: unique(streamerIds.filter((id) => !H.has(id) && qualifies(id))),
    remove: unique(holderIds.filter((id) => !(S.has(id) && qualifies(id)))),
    check: unique(unknownIds.filter((id) => !H.has(id) && !S.has(id))),
  };
}

/** @returns {'gone'|'role-missing'|'perms'|'error'} */
export function classifyRoleError(err) {
  const code = err?.code;
  if (code === 10007) return 'gone';          // Unknown Member
  if (code === 10011) return 'role-missing';  // Unknown Role
  if (code === 50013 || code === 50001) return 'perms'; // Missing Permissions / Missing Access
  return 'error';
}

/** Text for command replies. `r` is a SyncResult, or null while the sync is still running. */
export function formatSyncResult(r) {
  if (r == null) return '⏳ Still syncing in the background. Check `/liverole status` in a minute.';
  if (!r.ok) {
    switch (r.code) {
      case 'settings': return '⚠️ Could not sync right now (settings unavailable). It runs again on the next restart.';
      case 'off': return 'ℹ️ Not synced: the live role is off.';
      case 'no-role': return 'ℹ️ Not synced: no live role is set.';
      case 'role-missing': return '⚠️ Not synced: the live role was deleted.';
      case 'cannot-manage': return `⚠️ Not synced: ${r.reason}`;
      default: return '⚠️ Sync failed. Check the bot logs.';
    }
  }
  const parts = [];
  if (r.mode === 'clear') {
    parts.push(r.removed > 0 ? `Removed the live role from **${formatNumber(r.removed)}** member(s).` : 'Nobody had the live role.');
  } else if (r.added === 0 && r.removed === 0) {
    parts.push('Everyone is already in sync.');
  } else {
    parts.push(`Synced: gave it to **${formatNumber(r.added)}**, removed it from **${formatNumber(r.removed)}**.`);
  }
  if (r.failed > 0) {
    parts.push(`⚠️ **${formatNumber(r.failed)}** change(s) failed. Check that my role is above the live role and that I have **Manage Roles**.`);
  }
  if (r.deferred > 0) {
    parts.push(`ℹ️ **${formatNumber(r.deferred)}** more change(s) were left for later (at most ${LIVEROLE.SYNC_MAX_CHANGES} per sync). Run \`${r.mode === 'clear' ? '/liverole disable' : '/liverole enable'}\` again to continue.`);
  }
  if (r.addsBlocked) parts.push(`⚠️ Not giving the role to anyone: ${r.addsBlocked}`);
  if (r.complete === false) parts.push('ℹ️ Only cached members were checked. Members who are offline may keep the role until they next stream.');
  return parts.join('\n');
}

/** Problem lines for /liverole status, in the spec's order. */
export function liveRoleProblems({
  enabled, liveRoleId, roleExists, manage, dangerousNames = [], requiredRoleId, requiredRoleExists, holderCount = 0,
} = {}) {
  const out = [];
  if (enabled && !liveRoleId) out.push('⚠️ The module is on but no live role is set. Use `/liverole set`.');
  if (liveRoleId && !roleExists) out.push('⚠️ The live role was deleted. Pick a new one with `/liverole set`.');
  if (roleExists && manage && !manage.ok) out.push(`⚠️ ${manage.reason} I can't give or remove the live role until this is fixed.`);
  if (roleExists && dangerousNames.length > 0) {
    out.push(`⚠️ The live role has dangerous permissions (${dangerousNames.join(', ')}). I won't give it to anyone until they are removed.`);
  }
  if (requiredRoleId && !requiredRoleExists) out.push('⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.');
  if (!enabled && holderCount > 0) {
    out.push(`ℹ️ **${formatNumber(holderCount)}** member(s) still have the live role. \`/liverole disable\` removes it from them.`);
  }
  return out;
}

// ── Engine ────────────────────────────────────────────────────────────────────

// The required role "exists" when there is none, when it is @everyone, or when the guild has it. A deleted one fails closed.
function requiredRoleState(guild, settings) {
  const requiredRoleId = settings?.liveRoleRequiredRoleId ?? null;
  const exists = !requiredRoleId || requiredRoleId === guild.id || guild.roles.cache.has(requiredRoleId);
  return { requiredRoleId, exists };
}

async function reconcileLocked(guild, userId, { mode, settings, source }) {
  // 1. Role. A cache miss is never a deletion (rule 20): report it, keep the setting.
  const role = settings?.liveRoleId ? guild.roles.cache.get(settings.liveRoleId) : null;
  if (!role) {
    warn(guild.id, 'role-missing', `live role ${settings?.liveRoleId} not found; nothing is assigned until /liverole set picks a new one`);
    return 'role-missing';
  }

  // A presence decision that waited in the lock behind /liverole disable or /liverole set must not act on the old setup afterwards.
  if (source === 'presence') {
    const peeked = peekSettings(guild.id);
    if (peeked && (peeked.liveRoleEnabled !== true || peeked.liveRoleId !== settings.liveRoleId)) return 'none';
  }

  // 2. Member. A member built from a presence has no role data (partial), so fetch the real one.
  let member = guild.members.cache.get(userId) ?? null;
  if (!member || member.partial) {
    member = await guild.members.fetch({ user: userId, force: true }).catch(() => null);
  }
  if (!member) return 'gone';
  if (member.user?.bot) return 'none';

  // 3. Current state, read fresh inside the lock rather than from the event.
  const holds = member.roles.cache.has(role.id);
  const streaming = isStreaming(guild.presences.cache.get(userId));
  const { requiredRoleId, exists } = requiredRoleState(guild, settings);
  if (!exists) warn(guild.id, 'required-missing', `required role ${requiredRoleId} was deleted; nobody qualifies (fix with /liverole require)`);
  const qualifies = meetsRequirement({
    requiredRoleId, requiredRoleExists: exists, memberRoleIds: member.roles.cache, guildId: guild.id,
  });

  // 4. Idempotent: add only when missing, remove only when present. A stream stop removes the role
  // whatever the required role says.
  const action = planMember({ mode, streaming, qualifies, holds });
  if (!action) return 'none';

  // 5. Safety check before every Discord call. Adds are refused when the role gained dangerous permissions; removals never are.
  const check = checkAssignableRole(guild, role, { allowDangerous: action === 'remove' });
  if (!check.ok) {
    warn(guild.id, 'blocked', `cannot ${action} live role ${role.id}: ${check.reason}`);
    return 'blocked';
  }

  // 6. One role at a time, by id (R161: the array and set forms PATCH the whole role list).
  let reason;
  if (mode === 'clear') reason = REASONS.clear;
  else if (source === 'presence') reason = action === 'add' ? REASONS.presenceAdd : REASONS.presenceRemove;
  else reason = action === 'add' ? REASONS.syncAdd : REASONS.syncRemove;

  try {
    if (action === 'add') {
      await member.roles.add(role.id, reason);
      return 'added';
    }
    await member.roles.remove(role.id, reason);
    return 'removed';
  } catch (err) {
    // 7. Discord errors.
    const kind = classifyRoleError(err);
    if (kind === 'gone') {
      logger.debug(`[liverole] ${guild.id}: member ${userId} is gone (10007)`);
      return 'gone';
    }
    if (kind === 'role-missing') {
      warn(guild.id, 'role-missing', `live role ${role.id} no longer exists (10011); pick a new one with /liverole set`);
      return 'role-missing';
    }
    if (kind === 'perms') {
      warn(guild.id, 'perms', `Missing Permissions while trying to ${action} live role ${role.id} (code ${err.code}); move my role above it and give me Manage Roles`);
      return 'failed';
    }
    warn(guild.id, 'error', `${action} live role failed for ${userId}: ${err?.code ?? ''} ${err?.message ?? err}`);
    return 'failed';
  }
}

/**
 * Brings one member's live role in line with their current presence. Idempotent, serialised per member, never throws.
 * @returns {Promise<'added'|'removed'|'none'|'gone'|'blocked'|'failed'|'role-missing'>}
 */
export async function reconcileMember(guild, userId, { mode = 'sync', settings, source = 'presence' } = {}) {
  try {
    // Rule 20: an unavailable guild has empty caches, so every role and member would look deleted.
    if (guild.available === false) return 'none';
    const key = `${guild.id}:${userId}`;
    const coalesce = source === 'presence';
    if (coalesce) {
      if (waiting.has(key)) return 'none'; // a queued run reads the newest presence when it starts
      waiting.add(key);
    }
    return await memberLock.run(key, () => {
      if (coalesce) waiting.delete(key);
      return reconcileLocked(guild, userId, { mode, settings, source });
    });
  } catch (err) {
    logger.error('[liverole] reconcile failed', err);
    return 'failed';
  }
}

/**
 * How many non-bot members hold `role` but would lose it in a sync (not streaming, or not eligible). Cache only. Never throws.
 */
export function countLosingHolders(guild, role, settings) {
  try {
    const { requiredRoleId, exists } = requiredRoleState(guild, settings);
    let n = 0;
    for (const m of role.members.values()) {
      if (m.user?.bot) continue;
      const action = planMember({
        mode: 'sync',
        streaming: isStreaming(guild.presences.cache.get(m.id)),
        qualifies: meetsRequirement({ requiredRoleId, requiredRoleExists: exists, memberRoleIds: m.roles.cache, guildId: guild.id }),
        holds: true,
      });
      if (action === 'remove') n++;
    }
    return n;
  } catch (err) {
    logger.debug(`[liverole] countLosingHolders failed: ${err?.message ?? err}`);
    return 0;
  }
}

/**
 * Makes the member cache complete so `role.members` lists every holder. Never throws.
 * @returns {Promise<boolean>} whether the cache is believed complete
 */
export async function loadMembers(guild) {
  try {
    const total = guild.memberCount ?? 0;
    if (guild.members.cache.size >= total) return true;
    if (total > LIVEROLE.FULL_FETCH_MAX_MEMBERS) return false;
    if (!fetchThrottle(guild.id)) return false;
    const res = await fetchAllMembers(guild, {
      timeoutMs: LIVEROLE.FULL_FETCH_TIMEOUT_MS + 5_000,
      maxMembers: LIVEROLE.FULL_FETCH_MAX_MEMBERS,
    });
    if (!res.ok) logger.debug(`[liverole] ${guild.id}: full member fetch skipped (${res.reason})`);
    return res.ok === true;
  } catch (err) {
    logger.debug(`[liverole] loadMembers failed: ${err?.message ?? err}`);
    return false;
  }
}

/**
 * Whole-guild reconciliation. mode 'clear' removes the role from every holder. Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {{ mode?: 'sync'|'clear', reason?: string }} [opts]
 * @returns {Promise<{ ok: true, mode: string, added: number, removed: number, failed: number, deferred: number, addsBlocked: string|null, complete: boolean }
 *   | { ok: false, code: 'settings'|'off'|'no-role'|'role-missing'|'cannot-manage'|'error', reason?: string }>}
 */
export async function syncGuild(guild, { mode = 'sync', reason = 'startup' } = {}) {
  try {
    // Rule 20: an unavailable guild has empty caches; nothing may be concluded from them.
    if (guild.available === false) return { ok: false, code: 'error' };
    return await syncLock.run(guild.id, async () => {
      // Always the current values: a sync queued behind another one may run much later.
      const settings = await getSettingsSafe(guild.id);
      if (!settings) return { ok: false, code: 'settings' };
      if (mode === 'sync' && settings.liveRoleEnabled !== true) return { ok: false, code: 'off' };
      if (!settings.liveRoleId) return { ok: false, code: 'no-role' };
      const role = guild.roles.cache.get(settings.liveRoleId);
      if (!role) return { ok: false, code: 'role-missing' };
      const manage = checkAssignableRole(guild, role, { allowDangerous: true });
      if (!manage.ok) return { ok: false, code: 'cannot-manage', reason: manage.reason };
      // With dangerous permissions nothing is added, but removals still happen.
      const addCheck = mode === 'sync' ? checkAssignableRole(guild, role, { allowDangerous: false }) : { ok: true };

      const complete = await loadMembers(guild);

      const holderIds = [...role.members.filter((m) => !m.user.bot && !m.partial).keys()];
      const streamerIds = [];
      const unknownIds = [];
      for (const [id, p] of guild.presences.cache) {
        if (!isStreaming(p)) continue;
        const m = guild.members.cache.get(id);
        if (m?.user?.bot) continue;
        if (!m || m.partial) unknownIds.push(id);
        else streamerIds.push(id);
      }
      const { requiredRoleId, exists } = requiredRoleState(guild, settings);
      const qualifies = (id) => {
        const m = guild.members.cache.get(id);
        return !!m && !m.partial && meetsRequirement({
          requiredRoleId, requiredRoleExists: exists, memberRoleIds: m.roles.cache, guildId: guild.id,
        });
      };

      const plan = planSync({ mode, holderIds, streamerIds, unknownIds, qualifies });
      // Adds first, so streamers get their role even when the cap cuts the cleanup short.
      const queue = addCheck.ok ? [...plan.add, ...plan.check, ...plan.remove] : [...plan.remove];
      const work = queue.slice(0, LIVEROLE.SYNC_MAX_CHANGES);
      const deferred = queue.length - work.length;

      let added = 0;
      let removed = 0;
      let failed = 0;
      // One member at a time, never in parallel. Each call re-checks the member's live state inside the member lock.
      for (const id of work) {
        const out = await reconcileMember(guild, id, { mode, settings, source: 'sync' });
        if (out === 'added') added++;
        else if (out === 'removed') removed++;
        else if (out === 'failed' || out === 'blocked' || out === 'role-missing') failed++;
        if (out === 'role-missing') break; // the role vanished mid-sync
      }

      if (added + removed + failed + deferred > 0) {
        logger.info(`[liverole] ${guild.id} ${reason}: +${added} -${removed}${failed ? ` (${failed} failed)` : ''}${deferred ? `, ${deferred} deferred` : ''}${complete ? '' : ' (cached members only)'}`);
      }
      return { ok: true, mode, added, removed, failed, deferred, addsBlocked: addCheck.ok ? null : addCheck.reason, complete };
    });
  } catch (err) {
    logger.error('[liverole] sync failed', err);
    return { ok: false, code: 'error' };
  }
}

// ── Hooks ─────────────────────────────────────────────────────────────────────

async function onPresenceUpdate(ctx) {
  const settings = await ctx.getSettings(); // memoised; the runner already loaded it for gating
  if (!settings?.liveRoleEnabled || !settings.liveRoleId) return;
  const userId = ctx.newPresence?.userId ?? ctx.member?.id;
  if (!userId) return;
  await reconcileMember(ctx.guild, userId, { mode: 'sync', settings, source: 'presence' });
}

/** Gaining or losing the required role changes who qualifies without any presence update: re-check that member. */
async function onMemberUpdate(ctx) {
  const settings = await ctx.getSettings();
  const required = settings?.liveRoleRequiredRoleId;
  if (!settings?.liveRoleEnabled || !settings.liveRoleId || !required) return;
  if (ctx.oldMember.roles.cache.has(required) === ctx.newMember.roles.cache.has(required)) return;
  await reconcileMember(ctx.guild, ctx.newMember.id, { mode: 'sync', settings, source: 'sync' }); // audit reason: "not eligible"
}

async function onGuildReady(ctx) {
  const settings = await ctx.getSettings();
  if (!settings?.liveRoleEnabled || !settings.liveRoleId) return;
  // 'available' behaves exactly like 'startup'; only a bot join is labelled differently.
  await syncGuild(ctx.guild, { mode: 'sync', reason: ctx.reason === 'join' ? 'join' : 'startup' });
}

// Hook ids are the bare 'liverole' (HOOK_ORDER lists it). No hook is `always`: while the module is off the bot never touches
// the live role from events; /liverole disable is the cleanup path.
export const plugin = {
  id: 'liverole',
  module: 'liverole',
  hooks: [
    {
      event: 'presenceUpdate',
      id: 'liverole',
      filter: (ctx) => shouldHandlePresence(ctx.oldPresence, ctx.newPresence, peekSettings(ctx.guildId)),
      run: onPresenceUpdate,
      timeoutMs: 30_000,
    },
    {
      event: 'guildReady',
      id: 'liverole',
      run: onGuildReady,
      timeoutMs: 120_000, // a full member fetch plus up to 50 role edits can exceed the 10 s default
    },
    {
      event: 'guildMemberUpdate',
      id: 'liverole',
      // An uncached old member cannot be diffed (onMemberUpdate compares the required role).
      filter: (ctx) => ctx.oldKnown === true && !ctx.newMember?.user?.bot,
      run: onMemberUpdate,
      timeoutMs: 30_000,
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
