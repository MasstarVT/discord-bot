// Voice Roles: a member has a role for as long as they are in a linked voice channel (or in any voice channel of a linked
// category), and a few "any voice channel" roles while they are in voice at all. Settings: GuildSettings.voiceRoleLinks
// (`[{ channelId, roleId }]`), voiceRoleAnyRoleIds, voiceRoleIgnoreAfk. This file holds the rules (which roles a member
// should have, how they are changed, the startup/`/voicerole sync` reconcile) and the settings edits; the hooks are
// src/plugins/voice-roles.js, the admin command is src/commands/admin/voicerole.js.
//
// Every change recomputes the member's roles from their CURRENT voice state: never "add on join, remove on leave", because
// fast hops would race. Calls are one role at a time (a single-role PUT/DELETE; the array forms PATCH the member's whole
// role list from the cache and could undo a role another hook added at the same moment), serialised per member.
import { PermissionFlagsBits } from 'discord.js';
import { LIMITS } from '../config/constants.js';
import { updateSettingsWith } from './settingsService.js';
import { checkAssignableRole } from '../utils/interactionHelpers.js';
import { createKeyedMutex, createThrottle, mapLimit } from '../utils/async.js';
import logger from '../utils/logger.js';

export const VOICE_ROLES = Object.freeze({
  ADD_REASON: 'Voice Roles: joined a linked voice channel',
  REMOVE_REASON: 'Voice Roles: not in a linked voice channel',
  // Most members one reconcile pass looks at (voice members first, then cached holders of a managed role).
  RECONCILE_MAX: 1000,
  RECONCILE_CONCURRENCY: 3,
  // discord.js leaves `member.roles.cache` alone after a role call until the gateway echo arrives, so the roles this file
  // just changed are remembered for a short time (bounded), and a fast A -> B -> A hop sees its own earlier changes.
  OVERLAY_TTL_MS: 20_000,
  OVERLAY_MAX: 2000,
});

// "Unknown member" and "Unknown user": the member is gone, nothing more to do for them.
const LEFT_CODES = new Set([10007, 10013]);
// "Unknown role" and "Missing permissions": this role cannot be changed; the others still can.
const ROLE_FAIL_CODES = new Set([10011, 50013]);

const warnRole = createThrottle(3_600_000);
const lock = createKeyedMutex();

/** Runs `fn` while holding the member's lock (one member's role changes happen one at a time). */
export function runLocked(guildId, userId, fn) {
  return lock.run(`${guildId}:${userId}`, fn);
}

// ── Settings shapes ───────────────────────────────────────────────────────────

/** The link rows of a settings row, tolerant of junk (the dashboard and old rows are not trusted): `[{ channelId, roleId }]`. */
export function linksOf(settings) {
  const raw = Array.isArray(settings?.voiceRoleLinks) ? settings.voiceRoleLinks : [];
  const out = [];
  const seen = new Set();
  for (const l of raw) {
    if (typeof l?.channelId !== 'string' || typeof l?.roleId !== 'string' || !l.channelId || !l.roleId) continue;
    const key = `${l.channelId}:${l.roleId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ channelId: l.channelId, roleId: l.roleId });
  }
  return out;
}

/** The "any voice channel" role ids of a settings row (strings, no duplicates). */
export function anyRolesOf(settings) {
  const raw = Array.isArray(settings?.voiceRoleAnyRoleIds) ? settings.voiceRoleAnyRoleIds : [];
  return [...new Set(raw.filter((id) => typeof id === 'string' && id))];
}

/** True when anything is configured (a link or an any-role). */
export function hasVoiceRoles(settings) {
  return linksOf(settings).length > 0 || anyRolesOf(settings).length > 0;
}

/**
 * Every role this module owns: the roles of all links plus the any-list. The bot gives these to members in voice and takes
 * them from everyone else. Pure; Role Persistence can use it to leave these roles out of what it gives back.
 * @param {object} settings GuildSettings row
 * @returns {Set<string>} role ids
 */
export function managedRoleIds(settings) {
  return new Set([...linksOf(settings).map((l) => l.roleId), ...anyRolesOf(settings)]);
}

// ── Voice state and the roles a member should have ────────────────────────────

/**
 * The part of a voice state the rules need: `{ channelId, parentId }`, or null when the member is not in a voice channel.
 * @param {import('discord.js').VoiceState|null|undefined} state
 * @param {import('discord.js').Guild} guild
 */
export function voiceOf(state, guild) {
  const channelId = state?.channelId;
  if (!channelId) return null;
  const parentId = state.channel?.parentId ?? guild?.channels?.cache?.get(channelId)?.parentId ?? null;
  return { channelId, parentId };
}

/**
 * The member's voice position right now. The guild's voice-state cache is the truth (discord.js updates it before it emits
 * the event), so a queued change always acts on the latest state; `fallback` is used only when there is no such cache.
 */
function currentVoice(guild, userId, fallback) {
  const cache = guild?.voiceStates?.cache;
  if (!cache || typeof cache.get !== 'function') return fallback ?? null;
  return voiceOf(cache.get(userId), guild);
}

/**
 * The roles a member should have: the links whose channel is the member's channel or its category, plus the any-list while
 * in any voice channel. Nothing when not in voice, or in the server's AFK channel while `voiceRoleIgnoreAfk` is on.
 * @param {object} settings GuildSettings row
 * @param {import('discord.js').Guild} guild
 * @param {{ channelId: string, parentId: string|null }|null} voice
 * @returns {Set<string>} role ids
 */
export function desiredRoleIds(settings, guild, voice) {
  const want = new Set();
  if (!voice?.channelId) return want;
  if (settings?.voiceRoleIgnoreAfk !== false && guild?.afkChannelId && voice.channelId === guild.afkChannelId) return want;
  for (const l of linksOf(settings)) {
    if (l.channelId === voice.channelId || (voice.parentId && l.channelId === voice.parentId)) want.add(l.roleId);
  }
  for (const id of anyRolesOf(settings)) want.add(id);
  return want;
}

// ── Recently changed roles (the cache lags behind our own calls) ──────────────

const overlay = new Map(); // `${guildId}:${userId}` -> Map(roleId -> { held, until })

function heldNow(key, member, roleId, now) {
  const cached = member.roles?.cache?.has(roleId) === true;
  const mine = overlay.get(key);
  const entry = mine?.get(roleId);
  if (!entry) return cached;
  if (entry.until <= now || entry.held === cached) {
    mine.delete(roleId); // expired, or the cache caught up
    if (mine.size === 0) overlay.delete(key);
    return cached;
  }
  return entry.held;
}

function noteRole(key, roleId, held, now) {
  let mine = overlay.get(key);
  if (mine) {
    overlay.delete(key); // re-inserted below, so the oldest member is always first
  } else {
    while (overlay.size >= VOICE_ROLES.OVERLAY_MAX) overlay.delete(overlay.keys().next().value);
    mine = new Map();
  }
  mine.set(roleId, { held, until: now + VOICE_ROLES.OVERLAY_TTL_MS });
  overlay.set(key, mine);
}

// ── Changing a member's roles ─────────────────────────────────────────────────

/**
 * Brings one member's managed roles in line with their current voice state: adds the desired roles they lack, removes the
 * managed roles they hold that are not desired. Serialised per member. Skips bots and roles the bot cannot change (above
 * its highest role, managed, deleted; a role with a dangerous permission is never added). Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {import('discord.js').GuildMember} member
 * @param {object} settings GuildSettings row
 * @param {{ voice?: { channelId: string, parentId: string|null }|null, now?: number|null, alsoManaged?: Iterable<string> }} [opts]
 *   `voice`: used only when the guild has no voice-state cache; `now`: a fixed clock (tests). By default the clock is read at
 *   each step, not when the call was queued: a call that waited behind slow role calls must not note its changes as already
 *   expired. `alsoManaged`: extra roles treated as managed (the roles of links that were just dropped), so a holder who is not
 *   entitled to them by `settings` loses them.
 * @returns {Promise<{ status: 'ok'|'skipped'|'blocked'|'left'|'failed', added: string[], removed: string[], skipped: number }>}
 */
export async function applyMember(guild, member, settings, { voice = null, now = null, alsoManaged = null } = {}) {
  const result = { status: 'ok', added: [], removed: [], skipped: 0 };
  try {
    if (!guild || !member || member.user?.bot === true) return { ...result, status: 'skipped' };
    return await runLocked(guild.id, member.id, () => applyLocked(guild, member, settings, voice, now, result, alsoManaged));
  } catch (err) {
    logger.error(`[voice-roles] ${guild?.id}: could not update the roles of ${member?.id}`, err);
    return { ...result, status: 'failed' };
  }
}

async function applyLocked(guild, member, settings, fallbackVoice, fixedNow, result, alsoManaged = null) {
  const clock = () => fixedNow ?? Date.now();
  const key = `${guild.id}:${member.id}`;
  const want = desiredRoleIds(settings, guild, currentVoice(guild, member.id, fallbackVoice));
  const toAdd = [];
  const toRemove = [];
  for (const roleId of new Set([...managedRoleIds(settings), ...(alsoManaged ?? [])])) {
    const role = guild.roles?.cache?.get(roleId);
    if (!role) continue; // deleted (the roleDelete hook drops it from the settings)
    const held = heldNow(key, member, roleId, clock());
    if (want.has(roleId) && !held) toAdd.push(role);
    else if (!want.has(roleId) && held) toRemove.push(role);
  }
  if (!toAdd.length && !toRemove.length) return result;

  if (!guild.members?.me?.permissions?.has(PermissionFlagsBits.ManageRoles)) {
    if (warnRole(`${guild.id}:*:perm`)) logger.warn(`[voice-roles] ${guild.id}: cannot change voice roles, the bot lacks Manage Roles`);
    return { ...result, status: 'blocked' };
  }

  const steps = [
    ...toAdd.map((role) => ({ role, adding: true })),
    ...toRemove.map((role) => ({ role, adding: false })),
  ];
  for (const { role, adding } of steps) {
    // A role that cannot be given is skipped (and a role that became dangerous is never given); removing a role the bot
    // can reach is always fine.
    const chk = checkAssignableRole(guild, role, { allowDangerous: !adding });
    if (!chk.ok) {
      result.skipped++;
      if (warnRole(`${guild.id}:${role.id}:${adding ? 'add' : 'remove'}`)) {
        logger.warn(`[voice-roles] ${guild.id}: skipping role ${role.id}: ${chk.reason}`);
      }
      continue;
    }
    try {
      if (adding) await member.roles.add(role.id, VOICE_ROLES.ADD_REASON);
      else await member.roles.remove(role.id, VOICE_ROLES.REMOVE_REASON);
      noteRole(key, role.id, adding, clock());
      (adding ? result.added : result.removed).push(role.id);
    } catch (err) {
      if (LEFT_CODES.has(err?.code)) return { ...result, status: 'left' };
      if (ROLE_FAIL_CODES.has(err?.code)) {
        result.skipped++;
        if (warnRole(`${guild.id}:${role.id}:${adding ? 'add' : 'remove'}`)) {
          logger.warn(`[voice-roles] ${guild.id}: could not ${adding ? 'give' : 'take'} role ${role.id}: ${err?.message}`);
        }
        continue;
      }
      logger.warn(`[voice-roles] ${guild.id}: could not ${adding ? 'give' : 'take'} role ${role.id} for ${member.id}: ${err?.message}`);
      return { ...result, status: 'failed' };
    }
  }
  return result;
}

// ── Reconcile (startup and /voicerole sync) ───────────────────────────────────

const running = new Set(); // guild ids with a reconcile in progress

/** True while a reconcile of the guild is running (startup or `/voicerole sync`). */
export function isReconciling(guildId) {
  return running.has(guildId);
}

/**
 * The members one reconcile pass looks at, from caches only: everyone in a voice channel (bots excluded), then cached
 * holders of a managed role. At most `max`; `truncated` says more existed.
 */
function collectTargets(guild, settings, max) {
  const targets = new Map();
  let truncated = false;
  const take = (member) => {
    if (!member || member.user?.bot === true || targets.has(member.id)) return;
    if (targets.size >= max) { truncated = true; return; }
    targets.set(member.id, member);
  };
  for (const state of guild.voiceStates?.cache?.values() ?? []) {
    if (state?.channelId) take(state.member ?? guild.members?.cache?.get(state.id));
  }
  const managed = managedRoleIds(settings);
  if (managed.size) {
    for (const member of guild.members?.cache?.values() ?? []) {
      for (const roleId of member.roles?.cache?.keys() ?? []) {
        if (managed.has(roleId)) { take(member); break; }
      }
    }
  }
  return { list: [...targets.values()], truncated };
}

/**
 * Gives every member in voice their roles and takes the managed roles from cached holders who are not in voice. Bounded
 * (RECONCILE_MAX members, small concurrency), caches only (no member fetch), one pass per guild at a time. Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {object} settings GuildSettings row
 * @param {{ max?: number }} [opts]
 * @returns {Promise<{ busy: boolean, checked: number, changed: number, added: number, removed: number, failed: number, truncated: boolean }>}
 */
export async function reconcileGuild(guild, settings, { max = VOICE_ROLES.RECONCILE_MAX } = {}) {
  const out = { busy: false, checked: 0, changed: 0, added: 0, removed: 0, failed: 0, truncated: false };
  if (running.has(guild.id)) return { ...out, busy: true };
  running.add(guild.id);
  try {
    const { list, truncated } = collectTargets(guild, settings, max);
    out.truncated = truncated;
    out.checked = list.length;
    const results = await mapLimit(list, VOICE_ROLES.RECONCILE_CONCURRENCY, (m) => applyMember(guild, m, settings));
    for (const r of results) {
      const v = r.status === 'fulfilled' ? r.value : { status: 'failed', added: [], removed: [] };
      out.added += v.added.length;
      out.removed += v.removed.length;
      if (v.added.length || v.removed.length) out.changed++;
      if (v.status === 'failed' || v.status === 'blocked') out.failed++;
    }
  } catch (err) {
    logger.error(`[voice-roles] ${guild?.id}: reconcile failed`, err);
  } finally {
    running.delete(guild.id);
  }
  return out;
}

/**
 * A linked channel or category was deleted and its links dropped: the cached members who hold one of those links' roles
 * (or just got it, per the overlay) and are not entitled to it by `settings` (the row after the drop: another link or the
 * any-list) lose it, by the usual per-member rules. Bounded like reconcile (RECONCILE_MAX members, small concurrency, caches
 * only). Never throws.
 * @param {import('discord.js').Guild} guild
 * @param {object} settings GuildSettings row without the dropped links
 * @param {Iterable<string>} roleIds the roles of the dropped links
 * @param {{ max?: number }} [opts]
 * @returns {Promise<{ checked: number, removed: number, failed: number, truncated: boolean }>}
 */
export async function releaseDroppedRoles(guild, settings, roleIds, { max = VOICE_ROLES.RECONCILE_MAX } = {}) {
  const out = { checked: 0, removed: 0, failed: 0, truncated: false };
  const dropped = new Set([...(roleIds ?? [])].filter((id) => typeof id === 'string' && id));
  if (!guild || !dropped.size) return out;
  try {
    const targets = new Map();
    const take = (member) => {
      if (!member || member.user?.bot === true || targets.has(member.id)) return;
      if (targets.size >= max) { out.truncated = true; return; }
      targets.set(member.id, member);
    };
    for (const member of guild.members?.cache?.values() ?? []) {
      for (const roleId of dropped) {
        if (member.roles?.cache?.has(roleId)) { take(member); break; }
      }
    }
    const prefix = `${guild.id}:`;
    for (const [key, mine] of overlay) {
      if (!key.startsWith(prefix)) continue;
      if ([...dropped].some((id) => mine.get(id)?.held === true)) take(guild.members?.cache?.get(key.slice(prefix.length)));
    }
    const list = [...targets.values()];
    out.checked = list.length;
    const results = await mapLimit(list, VOICE_ROLES.RECONCILE_CONCURRENCY, (m) => applyMember(guild, m, settings, { alsoManaged: dropped }));
    for (const r of results) {
      const v = r.status === 'fulfilled' ? r.value : { status: 'failed', removed: [] };
      out.removed += v.removed.length;
      if (v.status === 'failed' || v.status === 'blocked') out.failed++;
    }
  } catch (err) {
    logger.error(`[voice-roles] ${guild?.id}: releasing the roles of a deleted channel failed`, err);
  }
  return out;
}

/** Forgets the overlay and the running set (tests). */
export function resetState() {
  overlay.clear();
  running.clear();
}

// ── Settings edits (command) ──────────────────────────────────────────────────

/**
 * Links a channel (voice, stage or category) to a role. The cap and the duplicate check run under the settings row lock.
 * @returns {Promise<'changed'|'same'|'full'>}
 */
export async function addLink(guildId, channelId, roleId) {
  let outcome = 'changed';
  await updateSettingsWith(guildId, (cur) => {
    const list = linksOf(cur);
    if (list.some((l) => l.channelId === channelId && l.roleId === roleId)) { outcome = 'same'; return null; }
    if (list.length >= LIMITS.VOICE_ROLE_LINKS) { outcome = 'full'; return null; }
    outcome = 'changed';
    return { voiceRoleLinks: [...list, { channelId, roleId }] };
  });
  return outcome;
}

/**
 * Removes a link. `stillManaged` says whether the role is still used by another link or the any-list (so the bot keeps
 * taking it from members outside voice).
 * @returns {Promise<{ outcome: 'changed'|'same', stillManaged: boolean }>}
 */
export async function removeLink(guildId, channelId, roleId) {
  let outcome = 'same';
  let stillManaged = false;
  await updateSettingsWith(guildId, (cur) => {
    const list = linksOf(cur);
    const kept = list.filter((l) => !(l.channelId === channelId && l.roleId === roleId));
    stillManaged = managedRoleIds({ voiceRoleLinks: kept, voiceRoleAnyRoleIds: cur.voiceRoleAnyRoleIds }).has(roleId);
    if (kept.length === list.length) return null;
    outcome = 'changed';
    return { voiceRoleLinks: kept };
  });
  return { outcome, stillManaged };
}

/** Adds a role to the any-voice-channel list. @returns {Promise<'changed'|'same'|'full'>} */
export async function addAnyRole(guildId, roleId) {
  let outcome = 'changed';
  await updateSettingsWith(guildId, (cur) => {
    const list = anyRolesOf(cur);
    if (list.includes(roleId)) { outcome = 'same'; return null; }
    if (list.length >= LIMITS.VOICE_ROLE_ANY) { outcome = 'full'; return null; }
    outcome = 'changed';
    return { voiceRoleAnyRoleIds: [...list, roleId] };
  });
  return outcome;
}

/** Removes a role from the any-voice-channel list. @returns {Promise<{ outcome: 'changed'|'same', stillManaged: boolean }>} */
export async function removeAnyRole(guildId, roleId) {
  let outcome = 'same';
  let stillManaged = false;
  await updateSettingsWith(guildId, (cur) => {
    const list = anyRolesOf(cur);
    const kept = list.filter((id) => id !== roleId);
    stillManaged = managedRoleIds({ voiceRoleLinks: cur.voiceRoleLinks, voiceRoleAnyRoleIds: kept }).has(roleId);
    if (kept.length === list.length) return null;
    outcome = 'changed';
    return { voiceRoleAnyRoleIds: kept };
  });
  return { outcome, stillManaged };
}

// ── Cleanup patches (pure; used by the always-on delete hooks) ────────────────

/** The settings patch that drops every link of a deleted channel or category, or null when there is none. */
export function patchDropChannel(cur, channelId) {
  const raw = Array.isArray(cur?.voiceRoleLinks) ? cur.voiceRoleLinks : [];
  const kept = raw.filter((l) => l?.channelId !== channelId);
  return kept.length === raw.length ? null : { voiceRoleLinks: kept };
}

/** The settings patch that drops a deleted role from the links and the any-list, or null when it is in neither. */
export function patchDropRole(cur, roleId) {
  const rawLinks = Array.isArray(cur?.voiceRoleLinks) ? cur.voiceRoleLinks : [];
  const rawAny = Array.isArray(cur?.voiceRoleAnyRoleIds) ? cur.voiceRoleAnyRoleIds : [];
  const links = rawLinks.filter((l) => l?.roleId !== roleId);
  const any = rawAny.filter((id) => id !== roleId);
  const patch = {};
  if (links.length !== rawLinks.length) patch.voiceRoleLinks = links;
  if (any.length !== rawAny.length) patch.voiceRoleAnyRoleIds = any;
  return Object.keys(patch).length ? patch : null;
}
