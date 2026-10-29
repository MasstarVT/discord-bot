// Server backups: the snapshot of a guild's roles, categories, channels and permission overwrites (version 1), and the
// restore engine that rebuilds what is missing. Used only by src/services/backupService.js.
//
// The restore NEVER deletes anything: it creates roles and channels, edits the ones that still exist (mode `full`
// only), moves them and upserts one permission overwrite at a time. Every decision is made from the snapshot and the
// guild's CURRENT state, so a batch that is repeated after a crash only does what is still undone (and adopts what the
// crashed run created before it could write down its id, but only after the audit log shows the bot made it for this restore).
//
// The restore engine takes a plain `env` ({ guild, snapshot, state, ... }) so tests drive it with fakes. All Discord
// calls go through `api()`: bounded in number and time, errors recorded in `state`, never thrown.
import { AuditLogEvent, ChannelType, OverwriteType, PermissionFlagsBits, PermissionsBitField, Routes } from 'discord.js';
import { withTimeout } from './async.js';
import { clampText } from './text.js';

export const SNAPSHOT_VERSION = 1;
export const RESTORE_MODES = Object.freeze(['missing', 'full']);
export const RESTORE_PHASES = Object.freeze(['roles', 'rolePositions', 'categories', 'channels', 'channelPositions', 'overwrites', 'finish']);

export const RESTORE_BUDGET = Object.freeze({
  CALLS: 10,               // Discord calls per batch (about)
  BATCH_MS: 18_000,        // no new item starts after this long (the scheduler's job timeout is 60 s)
  CALL_MS: 12_000,         // one call's own timeout (an item makes at most two calls)
  STALLS: 5,               // batches an item may time out in before it is reported and skipped
  ERRORS_KEPT: 20,         // error lines kept in the payload
  ERROR_CHARS: 200,
  POSITION_CHUNK: 50,      // channels per position call
  AUDIT_LIMIT: 100,        // audit log entries read to confirm that the bot made a role or channel (Discord's maximum)
  ID_MAP_MAX: 4000,       // entries accepted from a payload or a stored map
  LIST_MAX: 1500,          // roles / channels read from a stored snapshot
  OVERWRITES_MAX: 200,     // overwrites per channel read from a stored snapshot
});

/** Channel types a snapshot holds (threads, directory and the like are left out). */
const CHANNEL_TYPES = Object.freeze([
  ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildAnnouncement,
  ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia,
]);
const TEXT_LIKE = new Set([ChannelType.GuildText, ChannelType.GuildAnnouncement, ChannelType.GuildForum, ChannelType.GuildMedia]);
const VOICE_LIKE = new Set([ChannelType.GuildVoice, ChannelType.GuildStageVoice]);
const FORUM_LIKE = new Set([ChannelType.GuildForum, ChannelType.GuildMedia]);
const ARCHIVE_DURATIONS = new Set([60, 1440, 4320, 10080]);
const DIGITS = /^\d{1,20}$/;

export class SnapshotError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SnapshotError';
  }
}

// ── Permission bits ───────────────────────────────────────────────────────────

/** A bitfield (string, number or bigint) as a bigint; anything unreadable is 0. */
export function bits(value) {
  try {
    if (typeof value === 'bigint') return value;
    if (typeof value === 'number') return Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : 0n;
    const s = String(value ?? '0');
    return /^\d{1,20}$/.test(s) ? BigInt(s) : 0n;
  } catch {
    return 0n;
  }
}

/** Every permission bit the bot can hand out: its own (all of them with Administrator). */
export function botBits(me) {
  if (!me?.permissions) return 0n;
  if (me.permissions.has?.(PermissionFlagsBits.Administrator)) return PermissionsBitField.All;
  return bits(me.permissions.bitfield);
}

/**
 * What to write for a permission field. The bot can only give what it has, so wanted bits it lacks are dropped; bits it
 * cannot control that the target ALREADY has are kept (so nothing is taken away by accident).
 * @param {bigint} wanted bits the backup has
 * @param {bigint} bot bits the bot may set
 * @param {bigint} [current] bits the target has now (0 for a new target)
 * @returns {{ value: bigint, dropped: bigint }} `dropped`: wanted bits that could not be applied
 */
export function maskBits(wanted, bot, current = 0n) {
  return { value: (wanted & bot) | (current & ~bot), dropped: wanted & ~bot & ~current };
}

const toBits = (value) => (typeof value === 'bigint' ? value : bits(value));

// ── Capture ───────────────────────────────────────────────────────────────────

const text = (v, max) => (typeof v === 'string' ? clampText(v, max, '') : null);

function roleColour(role) {
  const c = role.colors?.primaryColor ?? role.color ?? 0;
  return Number.isInteger(c) && c >= 0 && c <= 0xffffff ? c : 0;
}

function captureOverwrites(channel) {
  const out = [];
  for (const o of channel.permissionOverwrites?.cache?.values?.() ?? []) {
    out.push({
      id: String(o.id),
      type: o.type === OverwriteType.Member || o.type === 'member' || o.type === 1 ? 'member' : 'role',
      allow: String(toBits(o.allow?.bitfield ?? 0n)),
      deny: String(toBits(o.deny?.bitfield ?? 0n)),
    });
  }
  return out;
}

function captureChannel(channel) {
  const out = {
    id: String(channel.id),
    type: channel.type,
    name: text(channel.name, 100) ?? 'channel',
    parentId: channel.parentId ? String(channel.parentId) : null,
    position: Number.isInteger(channel.rawPosition) ? channel.rawPosition : 0,
    overwrites: captureOverwrites(channel),
  };
  if (channel.type === ChannelType.GuildCategory) return out;
  if (TEXT_LIKE.has(channel.type)) {
    out.topic = text(channel.topic, 4096);
    out.nsfw = channel.nsfw === true;
    out.rateLimitPerUser = Number.isInteger(channel.rateLimitPerUser) ? channel.rateLimitPerUser : 0;
    out.defaultAutoArchiveDuration = ARCHIVE_DURATIONS.has(channel.defaultAutoArchiveDuration) ? channel.defaultAutoArchiveDuration : null;
  }
  if (VOICE_LIKE.has(channel.type)) {
    out.bitrate = Number.isInteger(channel.bitrate) ? channel.bitrate : null;
    out.userLimit = Number.isInteger(channel.userLimit) ? channel.userLimit : 0;
    out.rateLimitPerUser = Number.isInteger(channel.rateLimitPerUser) ? channel.rateLimitPerUser : 0;
  }
  if (FORUM_LIKE.has(channel.type)) {
    // Forum tags, simple ones only: a name, the moderated flag and a Unicode emoji (a custom emoji id would not survive).
    out.tags = (channel.availableTags ?? []).slice(0, 20).map((t) => ({
      name: text(t.name, 20) ?? 'tag',
      moderated: t.moderated === true,
      emojiName: t.emoji && !t.emoji.id && typeof t.emoji.name === 'string' ? t.emoji.name : null,
    }));
  }
  return out;
}

/**
 * Reads the guild's roles, categories, channels and permission overwrites from the cache (no Discord call). Version 1
 * holds no threads, messages, members, emojis, stickers, webhooks or bot settings.
 * @param {import('discord.js').Guild} guild
 * @param {Date} [now]
 * @returns {object} the snapshot (see parseSnapshot for the shape)
 */
export function captureGuild(guild, now = new Date()) {
  const roles = [];
  for (const role of guild.roles.cache.values()) {
    if (role.id === guild.id) continue;
    roles.push({
      id: String(role.id),
      name: text(role.name, 100) ?? 'role',
      color: roleColour(role),
      hoist: role.hoist === true,
      mentionable: role.mentionable === true,
      permissions: String(toBits(role.permissions?.bitfield ?? 0n)),
      position: Number.isInteger(role.rawPosition) ? role.rawPosition : 0,
      unicodeEmoji: typeof role.unicodeEmoji === 'string' ? role.unicodeEmoji : null,
      managed: role.managed === true,
    });
  }
  roles.sort((a, b) => a.position - b.position || (a.id < b.id ? -1 : 1));

  const categories = [];
  const channels = [];
  let skippedChannels = 0;
  for (const channel of guild.channels.cache.values()) {
    if (channel.isThread?.()) continue; // the cache holds threads too; version 1 keeps none
    if (channel.type === ChannelType.GuildCategory) categories.push(captureChannel(channel));
    else if (CHANNEL_TYPES.includes(channel.type)) channels.push(captureChannel(channel));
    else skippedChannels += 1;
  }
  const byPosition = (a, b) => a.position - b.position || (a.id < b.id ? -1 : 1);
  categories.sort(byPosition);
  channels.sort(byPosition);

  return {
    version: SNAPSHOT_VERSION,
    takenAt: now.toISOString(),
    guild: { id: String(guild.id), name: text(guild.name, 100) ?? 'server' },
    everyone: { permissions: String(toBits(guild.roles.everyone?.permissions?.bitfield ?? 0n)) },
    roles,
    categories,
    channels,
    skipped: { channels: skippedChannels },
  };
}

/** Size of a snapshot as stored, in bytes. */
export function snapshotBytes(data) {
  return Buffer.byteLength(JSON.stringify(data), 'utf8');
}

/** Counts shown in lists and info. */
export function snapshotStats(snapshot) {
  const sum = (list) => list.reduce((n, c) => n + c.overwrites.length, 0);
  return {
    roles: snapshot.roles.length,
    categories: snapshot.categories.length,
    channels: snapshot.channels.length,
    overwrites: sum(snapshot.categories) + sum(snapshot.channels),
  };
}

// ── Parse (a stored snapshot is read defensively) ─────────────────────────────

const int = (v, min, max, fallback) => (Number.isInteger(v) && v >= min && v <= max ? v : fallback);

function parseOverwrite(o) {
  if (!o || typeof o !== 'object' || typeof o.id !== 'string' || !DIGITS.test(o.id)) return null;
  const allow = bits(o.allow);
  const deny = bits(o.deny) & ~allow;
  return { id: o.id, type: o.type === 'member' ? 'member' : 'role', allow: String(allow), deny: String(deny) };
}

function parseChannel(c, isCategory) {
  if (!c || typeof c !== 'object' || typeof c.id !== 'string' || !DIGITS.test(c.id)) return null;
  const type = isCategory ? ChannelType.GuildCategory : c.type;
  if (isCategory ? c.type !== ChannelType.GuildCategory : !CHANNEL_TYPES.includes(type)) return null;
  const out = {
    id: c.id,
    type,
    name: text(c.name, 100) || 'channel',
    parentId: typeof c.parentId === 'string' && DIGITS.test(c.parentId) ? c.parentId : null,
    position: int(c.position, 0, 100000, 0),
    overwrites: (Array.isArray(c.overwrites) ? c.overwrites.slice(0, RESTORE_BUDGET.OVERWRITES_MAX) : []).map(parseOverwrite).filter(Boolean),
  };
  if (TEXT_LIKE.has(type)) {
    out.topic = text(c.topic, 4096) || null;
    out.nsfw = c.nsfw === true;
    out.rateLimitPerUser = int(c.rateLimitPerUser, 0, 21600, 0);
    out.defaultAutoArchiveDuration = ARCHIVE_DURATIONS.has(c.defaultAutoArchiveDuration) ? c.defaultAutoArchiveDuration : null;
  }
  if (VOICE_LIKE.has(type)) {
    out.bitrate = int(c.bitrate, 8000, 10_000_000, null);
    out.userLimit = int(c.userLimit, 0, 99, 0);
    out.rateLimitPerUser = int(c.rateLimitPerUser, 0, 21600, 0);
  }
  if (FORUM_LIKE.has(type)) {
    out.tags = (Array.isArray(c.tags) ? c.tags.slice(0, 20) : []).filter((t) => t && typeof t.name === 'string' && t.name)
      .map((t) => ({ name: text(t.name, 20), moderated: t.moderated === true, emojiName: typeof t.emojiName === 'string' && t.emojiName ? t.emojiName : null }));
  }
  return out;
}

function parseRole(r) {
  if (!r || typeof r !== 'object' || typeof r.id !== 'string' || !DIGITS.test(r.id)) return null;
  return {
    id: r.id,
    name: text(r.name, 100) || 'role',
    color: int(r.color, 0, 0xffffff, 0),
    hoist: r.hoist === true,
    mentionable: r.mentionable === true,
    permissions: String(bits(r.permissions)),
    position: int(r.position, 0, 100000, 0),
    unicodeEmoji: typeof r.unicodeEmoji === 'string' && r.unicodeEmoji ? r.unicodeEmoji : null,
    managed: r.managed === true,
  };
}

/** Old role/channel id -> the id the earlier restore gave it; only entries whose new id may still exist are useful. */
export function parseIdMap(raw) {
  const out = {};
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return out;
  let n = 0;
  for (const [k, v] of Object.entries(raw)) {
    if (n >= RESTORE_BUDGET.ID_MAP_MAX) break;
    if (DIGITS.test(k) && typeof v === 'string' && DIGITS.test(v)) { out[k] = v; n += 1; }
  }
  return out;
}

/**
 * Validates and normalises a stored snapshot.
 * @param {unknown} raw the `ServerBackup.data` JSON
 * @returns {{ version: 1, takenAt: string, guild: object, everyone: { permissions: string }, roles: object[],
 *   categories: object[], channels: object[], skipped: { channels: number }, idMap: Record<string, string> }}
 * @throws {SnapshotError} when it cannot be read
 */
export function parseSnapshot(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) throw new SnapshotError('The backup data cannot be read.');
  if (raw.version !== SNAPSHOT_VERSION) throw new SnapshotError(`This backup uses format version ${String(raw.version)}, which this bot cannot read.`);
  const list = (v) => (Array.isArray(v) ? v.slice(0, RESTORE_BUDGET.LIST_MAX) : []);
  const byPosition = (a, b) => a.position - b.position || (a.id < b.id ? -1 : 1);
  return {
    version: SNAPSHOT_VERSION,
    takenAt: typeof raw.takenAt === 'string' ? raw.takenAt : '',
    guild: { id: String(raw.guild?.id ?? ''), name: text(raw.guild?.name, 100) ?? '' },
    everyone: { permissions: String(bits(raw.everyone?.permissions)) },
    roles: list(raw.roles).map(parseRole).filter(Boolean).sort(byPosition),
    categories: list(raw.categories).map((c) => parseChannel(c, true)).filter(Boolean).sort(byPosition),
    channels: list(raw.channels).map((c) => parseChannel(c, false)).filter(Boolean).sort(byPosition),
    skipped: { channels: int(raw.skipped?.channels, 0, 100000, 0) },
    idMap: parseIdMap(raw.idMap),
  };
}

/** The snapshot as exported: without the bookkeeping a restore adds. */
export function exportable(data) {
  if (!data || typeof data !== 'object') return data;
  const { idMap: _ids, ...rest } = data;
  return rest;
}

// ── Restore state ─────────────────────────────────────────────────────────────

const COUNT_KEYS = Object.freeze([
  'rolesCreated', 'rolesUpdated', 'rolesKept', 'rolesSkipped',
  'categoriesCreated', 'categoriesUpdated', 'categoriesKept', 'categoriesSkipped',
  'channelsCreated', 'channelsUpdated', 'channelsKept', 'channelsSkipped',
  'overwritesSet', 'overwritesKept', 'overwritesSkipped', 'positionsSet', 'positionsLimited', 'masked', 'errors',
]);

/**
 * The payload of a `backup.restore` job at the start.
 * @param {{ backupId: string, mode: string, startedById?: string|null, channelId?: string|null, idMap?: object, now?: number }} p
 */
export function newRestoreState({ backupId, mode, startedById = null, channelId = null, idMap = {}, now = Date.now() }) {
  return {
    backupId, mode, phase: 'roles', cursor: 0, stalls: 0, retry: false,
    idMap: parseIdMap(idMap), made: [], counts: {}, errors: [], dropped: '0',
    startedById, channelId, startedAt: now,
  };
}

/**
 * Validates a job payload.
 * @param {unknown} raw
 * @returns {object|null} the state, or null when the payload is unusable (a permanent failure)
 */
export function normaliseRestoreState(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  if (typeof raw.backupId !== 'string' || !raw.backupId || !RESTORE_MODES.includes(raw.mode)) return null;
  if (!RESTORE_PHASES.includes(raw.phase)) return null;
  const counts = {};
  for (const k of COUNT_KEYS) counts[k] = int(raw.counts?.[k], 0, 1e9, 0);
  return {
    backupId: raw.backupId,
    mode: raw.mode,
    phase: raw.phase,
    cursor: int(raw.cursor, 0, 1e9, 0),
    stalls: int(raw.stalls, 0, 1000, 0),
    retry: raw.retry === true, // set by the job when this batch repeats one whose progress was lost (see adoptRole)
    idMap: parseIdMap(raw.idMap),
    made: (Array.isArray(raw.made) ? raw.made : []).filter((id) => typeof id === 'string' && DIGITS.test(id)).slice(0, RESTORE_BUDGET.ID_MAP_MAX),
    counts,
    errors: (Array.isArray(raw.errors) ? raw.errors : []).filter((e) => typeof e === 'string').slice(0, RESTORE_BUDGET.ERRORS_KEPT),
    dropped: String(bits(raw.dropped)),
    startedById: typeof raw.startedById === 'string' ? raw.startedById : null,
    channelId: typeof raw.channelId === 'string' ? raw.channelId : null,
    startedAt: int(raw.startedAt, 0, Number.MAX_SAFE_INTEGER, Date.now()),
  };
}

const bump = (state, key, n = 1) => { state.counts[key] = (state.counts[key] ?? 0) + n; };

function noteError(state, line) {
  bump(state, 'errors');
  if (state.errors.length < RESTORE_BUDGET.ERRORS_KEPT) state.errors.push(clampText(line, RESTORE_BUDGET.ERROR_CHARS));
}

function noteDropped(state, dropped) {
  if (dropped === 0n) return;
  bump(state, 'masked');
  state.dropped = String(bits(state.dropped) | dropped);
}

// ── The environment of one batch (or of a preview) ────────────────────────────

/**
 * @param {{ guild: object, snapshot: object, state: object, me?: object|null, reason?: string }} p
 */
export function makeEnv({ guild, snapshot, state, me = null, reason = 'Server backup restore' }) {
  const member = me ?? guild.members?.me ?? null;
  return {
    guild, snapshot, state, reason, mode: state.mode,
    me: member,
    botB: botBits(member),
    botTop: Number.isInteger(member?.roles?.highest?.rawPosition) ? member.roles.highest.rawPosition : 0,
    canManageRoles: Boolean(member?.permissions?.has?.(PermissionFlagsBits.ManageRoles)),
    canManageChannels: Boolean(member?.permissions?.has?.(PermissionFlagsBits.ManageChannels)),
    roleIcons: Boolean(guild.features?.includes?.('ROLE_ICONS')),
    madeSet: new Set(state.made),
    audit: new Map(), // audit log event -> ids the bot made for this restore (read once per batch, see madeByRestore)
    originalRoleIds: new Set(snapshot.roles.map((r) => r.id)),
    originalChannelIds: new Set([...snapshot.categories, ...snapshot.channels].map((c) => c.id)),
    budget: { calls: 0, max: RESTORE_BUDGET.CALLS, deadline: Date.now() + RESTORE_BUDGET.BATCH_MS },
    timedOut: false,
  };
}

const hasRoom = (env) => env.budget.calls < env.budget.max && Date.now() < env.budget.deadline;

function findRole(env, oldId) {
  const cache = env.guild.roles.cache;
  const mapped = env.state.idMap[oldId];
  return cache.get(oldId) ?? (mapped ? cache.get(mapped) : undefined) ?? null;
}

function findChannel(env, oldId) {
  const cache = env.guild.channels.cache;
  const mapped = env.state.idMap[oldId];
  return cache.get(oldId) ?? (mapped ? cache.get(mapped) : undefined) ?? null;
}

const describeError = (err) => {
  const code = err?.code != null && err.code !== '' ? ` (${err.code})` : '';
  return `${clampText(String(err?.message ?? err), 120)}${code}`;
};

/**
 * One bounded Discord call. Never throws: a failure is recorded in the state (unless `quiet`) and `{ ok: false }` returned.
 */
async function api(env, label, fn, { quiet = false } = {}) {
  env.budget.calls += 1;
  const left = env.budget.deadline - Date.now();
  const ms = Math.max(2000, Math.min(RESTORE_BUDGET.CALL_MS, left + 5000));
  try {
    return { ok: true, value: await withTimeout(Promise.resolve().then(fn), ms, label) };
  } catch (err) {
    if (err?.name === 'TimeoutError') {
      // The call may still go through later. The item is tried again in the next batch (see stalled) and may adopt what appeared.
      env.timedOut = true;
      return { ok: false, error: err, timeout: true };
    }
    if (!quiet) noteError(env.state, `${label}: ${describeError(err)}`);
    return { ok: false, error: err };
  }
}

/**
 * After an item: true when a call timed out and the batch should end with the cursor where it is (the item is retried
 * next batch, a few times); otherwise the item is done, failed or not.
 */
function stalled(env) {
  const { state } = env;
  if (!env.timedOut) {
    state.stalls = 0;
    return false;
  }
  env.timedOut = false;
  state.stalls += 1;
  if (state.stalls <= RESTORE_BUDGET.STALLS) return true;
  state.stalls = 0;
  noteError(state, 'Discord did not answer in time, so this item was skipped.');
  return false;
}

// ── Decisions (shared by the preview and the job) ─────────────────────────────

function roleCreateData(env, r) {
  const { value, dropped } = maskBits(bits(r.permissions), env.botB, 0n);
  const data = {
    name: r.name, colors: { primaryColor: r.color }, hoist: r.hoist, mentionable: r.mentionable,
    permissions: value, reason: env.reason,
  };
  if (r.unicodeEmoji && env.roleIcons) data.unicodeEmoji = r.unicodeEmoji;
  return { data, dropped };
}

function roleChanges(env, existing, r) {
  const data = {};
  if (existing.name !== r.name) data.name = r.name;
  if ((existing.colors?.primaryColor ?? existing.color ?? 0) !== r.color) data.colors = { primaryColor: r.color };
  if ((existing.hoist === true) !== r.hoist) data.hoist = r.hoist;
  if ((existing.mentionable === true) !== r.mentionable) data.mentionable = r.mentionable;
  const current = toBits(existing.permissions?.bitfield ?? 0n);
  const { value, dropped } = maskBits(bits(r.permissions), env.botB, current);
  if (value !== current) data.permissions = value;
  if (env.roleIcons && (existing.unicodeEmoji ?? null) !== r.unicodeEmoji) data.unicodeEmoji = r.unicodeEmoji;
  return { data: Object.keys(data).length ? data : null, dropped };
}

/** What to do with one snapshot role: create, update (mode full), keep or skip. */
export function decideRole(env, r) {
  const existing = findRole(env, r.id);
  if (r.managed) return { action: existing ? 'keep' : 'skip', existing, why: 'managed' }; // an integration's role cannot be made
  if (!existing) {
    const { data, dropped } = roleCreateData(env, r);
    return { action: 'create', data, dropped };
  }
  if (env.mode !== 'full') return { action: 'keep', existing };
  if (existing.managed || !(existing.rawPosition < env.botTop)) return { action: 'skip', existing, why: 'above me' };
  const { data, dropped } = roleChanges(env, existing, r);
  return data ? { action: 'update', existing, data: { ...data, reason: env.reason }, dropped } : { action: 'keep', existing, dropped };
}

function mappedParent(env, c) {
  return c.parentId ? (findChannel(env, c.parentId)?.id ?? null) : null;
}

/**
 * The overwrites a recreated channel is CREATED with, so a private channel is never public (and never locks the bot out)
 * while the overwrites phase is still to come or after a restore that stopped early: @everyone, roles that exist now, and
 * the bot's own member overwrite. Masked like every other write; Discord lets only administrators put Manage Permissions
 * in a new channel's overwrites, so a bot without Administrator leaves that bit to the overwrites phase. Other members'
 * overwrites follow in that phase (they need a member lookup).
 */
function createOverwrites(env, c) {
  if (!env.canManageRoles) return [];
  const keep = env.me?.permissions?.has?.(PermissionFlagsBits.Administrator) ? ~0n : ~PermissionFlagsBits.ManageRoles;
  const out = [];
  for (const ow of c.overwrites) {
    let target = null;
    if (ow.type === 'role') target = ow.id === env.guild.id ? ow.id : (findRole(env, ow.id)?.id ?? null);
    else if (env.me?.id && ow.id === String(env.me.id)) target = ow.id;
    if (!target) continue;
    const allow = maskBits(bits(ow.allow), env.botB, 0n).value & keep;
    const deny = maskBits(bits(ow.deny), env.botB, 0n).value & keep;
    if (allow === 0n && deny === 0n) continue;
    out.push({ id: target, type: ow.type === 'member' ? OverwriteType.Member : OverwriteType.Role, allow, deny });
    if (out.length >= 100) break;
  }
  return out;
}

function channelCreateData(env, c) {
  const data = { name: c.name, type: c.type, reason: env.reason };
  const parent = mappedParent(env, c);
  if (parent) data.parent = parent;
  data.position = c.position;
  const overwrites = createOverwrites(env, c);
  if (overwrites.length) data.permissionOverwrites = overwrites;
  if (TEXT_LIKE.has(c.type)) {
    if (c.topic) data.topic = c.topic;
    if (c.nsfw) data.nsfw = true;
    if (c.rateLimitPerUser) data.rateLimitPerUser = c.rateLimitPerUser;
    if (c.defaultAutoArchiveDuration) data.defaultAutoArchiveDuration = c.defaultAutoArchiveDuration;
  }
  if (VOICE_LIKE.has(c.type)) {
    if (c.bitrate) data.bitrate = Math.max(8000, Math.min(c.bitrate, env.guild.maximumBitrate ?? c.bitrate));
    if (c.userLimit) data.userLimit = c.userLimit;
    if (c.rateLimitPerUser) data.rateLimitPerUser = c.rateLimitPerUser;
  }
  if (FORUM_LIKE.has(c.type) && c.tags?.length) {
    data.availableTags = c.tags.map((t) => ({ name: t.name, moderated: t.moderated, emoji: t.emojiName ? { id: null, name: t.emojiName } : null }));
  }
  return data;
}

function channelChanges(env, existing, c) {
  const data = {};
  if (existing.name !== c.name) data.name = c.name;
  if (TEXT_LIKE.has(c.type)) {
    if ((existing.topic ?? null) !== (c.topic ?? null)) data.topic = c.topic ?? '';
    if ((existing.nsfw === true) !== (c.nsfw === true)) data.nsfw = c.nsfw === true;
    if ((existing.rateLimitPerUser ?? 0) !== c.rateLimitPerUser) data.rateLimitPerUser = c.rateLimitPerUser;
    if (c.defaultAutoArchiveDuration && existing.defaultAutoArchiveDuration !== c.defaultAutoArchiveDuration) data.defaultAutoArchiveDuration = c.defaultAutoArchiveDuration;
  }
  if (VOICE_LIKE.has(c.type)) {
    const bitrate = c.bitrate ? Math.max(8000, Math.min(c.bitrate, env.guild.maximumBitrate ?? c.bitrate)) : null;
    if (bitrate && existing.bitrate !== bitrate) data.bitrate = bitrate;
    if ((existing.userLimit ?? 0) !== c.userLimit) data.userLimit = c.userLimit;
  }
  const parent = mappedParent(env, c);
  if (parent && existing.parentId !== parent) data.parent = parent;
  return Object.keys(data).length ? data : null;
}

/** What to do with one snapshot category or channel: create, update (mode full), keep or skip. */
export function decideChannel(env, c) {
  const existing = findChannel(env, c.id);
  if (!existing) return { action: 'create', data: channelCreateData(env, c) };
  if (env.mode !== 'full') return { action: 'keep', existing };
  if (existing.manageable === false) return { action: 'skip', existing, why: 'not manageable' };
  const data = channelChanges(env, existing, c);
  return data ? { action: 'update', existing, data: { ...data, reason: env.reason } } : { action: 'keep', existing };
}

/** A role overwrite whose role was RECREATED: the original id is gone and the id map names its new role, which exists. */
function isRecreatedRole(env, ow) {
  if (ow.type !== 'role' || ow.id === env.guild.id || env.guild.roles.cache.has(ow.id)) return false;
  const mapped = env.state.idMap[ow.id];
  return Boolean(mapped) && env.guild.roles.cache.has(mapped);
}

/** What to do with one permission overwrite of the snapshot. `item`: { channelId, ow }. */
export function decideOverwrite(env, item) {
  const channel = findChannel(env, item.channelId);
  if (!channel) return { action: 'skip', why: 'channel missing' };
  const { ow } = item;
  // Mode missing leaves the channels that still exist as they are, except that a role it RECREATED gets its overwrite back
  // there (a recreated Muted role needs its channel denies): added only where the channel has none for that role, never
  // changing or removing an existing one.
  const additive = env.mode !== 'full' && !env.madeSet.has(channel.id);
  if (additive && !isRecreatedRole(env, ow)) return { action: 'untouched' };
  let target = null;
  if (ow.type === 'role') target = ow.id === env.guild.id ? ow.id : (findRole(env, ow.id)?.id ?? null);
  else target = ow.id;
  if (!target) return { action: 'skip', why: 'role gone' };
  const current = channel.permissionOverwrites?.cache?.get?.(target) ?? null;
  if (additive && current) return { action: 'untouched' };
  const curAllow = toBits(current?.allow?.bitfield ?? 0n);
  const curDeny = toBits(current?.deny?.bitfield ?? 0n);
  const allow = maskBits(bits(ow.allow), env.botB, curAllow);
  const deny = maskBits(bits(ow.deny), env.botB, curDeny);
  const dropped = allow.dropped | deny.dropped;
  if (current ? allow.value === curAllow && deny.value === curDeny : allow.value === 0n && deny.value === 0n) {
    return { action: 'keep', dropped };
  }
  return { action: 'set', channel, target, type: ow.type, allow: allow.value, deny: deny.value, dropped };
}

// ── Work lists (from the snapshot only, so a cursor means the same thing in every run) ──

export function roleItems(env) {
  const items = [];
  if (env.mode === 'full') items.push({ kind: 'everyone' });
  for (const role of env.snapshot.roles) items.push({ kind: 'role', role });
  return items;
}

export function overwriteItems(snapshot) {
  const items = [];
  for (const c of [...snapshot.categories, ...snapshot.channels]) {
    for (const ow of c.overwrites) items.push({ channelId: c.id, ow });
  }
  return items;
}

// ── Preview ───────────────────────────────────────────────────────────────────

/**
 * What a restore would do right now, counted with the same decisions the job uses. No Discord call.
 * @param {{ guild: object, snapshot: object, mode: string, idMap?: object }} p
 */
export function previewRestore({ guild, snapshot, mode, idMap = {} }) {
  const state = { mode, idMap: parseIdMap(idMap), made: [], counts: {}, errors: [], dropped: '0', startedAt: 0 };
  const env = makeEnv({ guild, snapshot, state });
  const out = {
    mode,
    roles: { create: 0, update: 0, keep: 0, skip: 0 },
    categories: { create: 0, update: 0, keep: 0, skip: 0 },
    channels: { create: 0, update: 0, keep: 0, skip: 0 },
    overwrites: 0, masked: 0, dropped: 0n, unsupported: snapshot.skipped.channels,
    managedMissing: 0, canManageRoles: env.canManageRoles, canManageChannels: env.canManageChannels,
    roleTotalAfter: guild.roles.cache.size, channelTotalAfter: guild.channels.cache.size,
    calls: 0,
  };
  const willHaveRole = new Set();
  const createdRoles = new Set();
  const noteDrop = (d) => { if (d) { out.masked += 1; out.dropped |= d; } };

  for (const item of roleItems(env)) {
    if (item.kind === 'everyone') {
      const cur = toBits(guild.roles.everyone?.permissions?.bitfield ?? 0n);
      const { value, dropped } = maskBits(bits(snapshot.everyone.permissions), env.botB, cur);
      if (value !== cur) { out.roles.update += 1; out.calls += 1; }
      noteDrop(dropped);
      continue;
    }
    const d = decideRole(env, item.role);
    out.roles[d.action] += 1;
    if (d.action === 'skip' && d.why === 'managed') out.managedMissing += 1;
    if (d.action === 'create') { out.calls += 1; willHaveRole.add(item.role.id); createdRoles.add(item.role.id); noteDrop(d.dropped); }
    if (d.action === 'update') { out.calls += 1; noteDrop(d.dropped); }
    if (d.existing) willHaveRole.add(item.role.id);
  }
  out.roleTotalAfter += out.roles.create;
  if (out.roles.create || (mode === 'full' && out.roles.update)) out.calls += 1; // the position call

  const willHaveChannel = new Set();
  const created = new Set();
  for (const [bucket, list] of [['categories', snapshot.categories], ['channels', snapshot.channels]]) {
    for (const c of list) {
      const d = decideChannel(env, c);
      out[bucket][d.action] += 1;
      if (d.action === 'create') { out.calls += 1; created.add(c.id); }
      if (d.action === 'update') out.calls += 1;
      if (d.existing || d.action === 'create') willHaveChannel.add(c.id);
    }
  }
  out.channelTotalAfter += out.categories.create + out.channels.create;
  if (mode === 'full') out.calls += Math.ceil((snapshot.categories.length + snapshot.channels.length) / RESTORE_BUDGET.POSITION_CHUNK);

  for (const item of overwriteItems(snapshot)) {
    if (!willHaveChannel.has(item.channelId)) continue;
    const { ow } = item;
    const roleTarget = ow.type === 'role' && ow.id !== guild.id;
    if (mode !== 'full' && !created.has(item.channelId)) {
      // A channel that still exists only gets the overwrites of RECREATED roles, where it has none for that role.
      if (createdRoles.has(ow.id)) {
        const allow = maskBits(bits(ow.allow), env.botB, 0n);
        const deny = maskBits(bits(ow.deny), env.botB, 0n);
        noteDrop(allow.dropped | deny.dropped);
        if (allow.value !== 0n || deny.value !== 0n) { out.overwrites += 1; out.calls += 1; }
      } else if (roleTarget) {
        const d = decideOverwrite(env, item); // a role an earlier restore recreated (id map): additive
        if (d.action === 'set') { out.overwrites += 1; out.calls += 1; noteDrop(d.dropped); }
      }
      continue;
    }
    if (roleTarget && !willHaveRole.has(ow.id) && !guild.roles.cache.has(ow.id)) continue; // the role is gone for good
    const existsNow = !roleTarget || Boolean(findRole(env, ow.id));
    if (created.has(item.channelId) || !existsNow) {
      // A new channel, or a role that will be made first: nothing to compare with, the whole overwrite is written.
      const allow = maskBits(bits(ow.allow), env.botB, 0n);
      const deny = maskBits(bits(ow.deny), env.botB, 0n);
      noteDrop(allow.dropped | deny.dropped);
      if (allow.value !== 0n || deny.value !== 0n) { out.overwrites += 1; out.calls += 1; }
      continue;
    }
    const d = decideOverwrite(env, item);
    if (d.action === 'set') { out.overwrites += 1; out.calls += 1; noteDrop(d.dropped); }
  }
  return out;
}

// ── Adopting what a crashed run made ──────────────────────────────────────────
// A role or channel that an earlier try of THIS restore made, but whose id was never saved (a create that timed out, or a
// batch whose progress was lost in a crash), is used instead of being made twice. Never by name alone: a same-named role or
// channel made by someone else (a member with Manage Roles, say) would be raised, ordered and given the overwrites of the
// original. So it takes an earlier try (mayAdopt, never the first one), an audit log entry that shows the bot made it for this
// restore, and then the name (and type and parent) of the snapshot item. Anything unconfirmed is made again: a rare duplicate
// after a crash is safer than adopting a stranger's.

/** True while the item at the cursor may exist already from an earlier try: its create timed out (`stalls`), or this batch repeats one whose progress was lost (`retry`). */
const mayAdopt = (env) => env.state.stalls > 0 || env.state.retry === true;

/**
 * Ids of what the bot made for this restore since it started, from the audit log (CREATE entries of `event`, executed by the
 * bot with this restore's reason). Read at most once per event per batch; empty when the log cannot be read, which means
 * nothing is adopted.
 */
async function madeByRestore(env, event) {
  const known = env.audit.get(event);
  if (known) return known;
  const ids = new Set();
  env.audit.set(event, ids); // a failed or timed-out read is not repeated inside the batch
  const meId = env.me?.id ?? env.guild.client?.user?.id;
  if (!meId) return ids;
  const res = await api(env, 'Audit log', () => env.guild.fetchAuditLogs({ type: event, limit: RESTORE_BUDGET.AUDIT_LIMIT }), { quiet: true });
  if (!res.ok) return ids;
  for (const entry of res.value?.entries?.values?.() ?? []) {
    if (entry?.targetId && entry.executorId === meId && entry.reason === env.reason && entry.createdTimestamp >= env.state.startedAt) {
      ids.add(String(entry.targetId));
    }
  }
  return ids;
}

/** A role the bot made for this restore, with the wanted name, that no snapshot role has claimed. */
async function adoptRole(env, r) {
  if (!mayAdopt(env)) return null;
  const taken = new Set(Object.values(env.state.idMap));
  const named = [...env.guild.roles.cache.values()].filter((role) => role.id !== env.guild.id && !role.managed
    && !taken.has(role.id) && !env.originalRoleIds.has(role.id) && role.name === r.name);
  if (!named.length) return null;
  const made = await madeByRestore(env, AuditLogEvent.RoleCreate);
  return named.find((role) => made.has(role.id)) ?? null;
}

/** A channel the bot made for this restore, with the wanted name, type and parent, that no snapshot channel has claimed. */
async function adoptChannel(env, c) {
  if (!mayAdopt(env)) return null;
  const taken = new Set(Object.values(env.state.idMap));
  const parent = mappedParent(env, c);
  const named = [...env.guild.channels.cache.values()].filter((ch) => !taken.has(ch.id) && !env.originalChannelIds.has(ch.id)
    && !ch.isThread?.() && ch.type === c.type && ch.name === c.name && (ch.parentId ?? null) === parent);
  if (!named.length) return null;
  const made = await madeByRestore(env, AuditLogEvent.ChannelCreate);
  return named.find((ch) => made.has(ch.id)) ?? null;
}

function remember(env, oldId, created) {
  env.state.idMap[oldId] = created.id;
  if (!env.state.made.includes(created.id)) env.state.made.push(created.id);
  env.madeSet.add(created.id);
}

// ── Phases ────────────────────────────────────────────────────────────────────

async function stepRoles(env) {
  const { state } = env;
  const items = roleItems(env);
  if (!env.canManageRoles) {
    if (state.cursor < items.length) {
      bump(state, 'rolesSkipped', items.length - state.cursor);
      noteError(state, 'I need the Manage Roles permission, so no roles were restored.');
    }
    state.cursor = items.length;
    return true;
  }
  while (state.cursor < items.length) {
    if (!hasRoom(env)) return false;
    const item = items[state.cursor];
    if (item.kind === 'everyone') {
      const everyone = env.guild.roles.everyone;
      const cur = toBits(everyone?.permissions?.bitfield ?? 0n);
      const { value, dropped } = maskBits(bits(env.snapshot.everyone.permissions), env.botB, cur);
      if (everyone && value !== cur) {
        const res = await api(env, '@everyone permissions', () => env.guild.roles.edit(everyone, { permissions: value, reason: env.reason }));
        if (res.ok) { noteDropped(state, dropped); bump(state, 'rolesUpdated'); }
      }
    } else {
      await restoreRole(env, item.role);
    }
    if (stalled(env)) return false;
    state.cursor += 1;
  }
  return true;
}

async function restoreRole(env, r) {
  const { state } = env;
  const d = decideRole(env, r);
  if (d.action === 'keep') {
    noteDropped(state, d.dropped ?? 0n);
    return void bump(state, 'rolesKept');
  }
  if (d.action === 'skip') return void bump(state, 'rolesSkipped'); // an integration's role, or one above the bot
  if (d.action === 'update') {
    const res = await api(env, `Role "${r.name}"`, () => env.guild.roles.edit(d.existing, d.data));
    if (res.ok) { noteDropped(state, d.dropped); bump(state, 'rolesUpdated'); }
    return;
  }
  const adopted = await adoptRole(env, r);
  if (adopted) {
    remember(env, r.id, adopted);
    return void bump(state, 'rolesCreated');
  }
  const res = await api(env, `Role "${r.name}"`, () => env.guild.roles.create(d.data));
  if (res.ok && res.value?.id) {
    remember(env, r.id, res.value);
    noteDropped(state, d.dropped);
    bump(state, 'rolesCreated');
  }
}

async function stepRolePositions(env) {
  const { state, guild } = env;
  if (state.cursor > 0 || !env.canManageRoles) return true;
  if (!hasRoom(env)) return false;
  const top = env.botTop;
  let entries = [];
  let limited = 0; // recreated roles that were above where the bot may place a role
  if (env.mode === 'missing') {
    // The recreated roles go back to the places they had, as far as the bot's own role allows.
    for (const r of env.snapshot.roles) {
      const role = findRole(env, r.id);
      if (!role || !env.madeSet.has(role.id)) continue;
      const position = Math.max(1, Math.min(r.position, top - 1));
      if (r.position > top - 1) limited += 1;
      if (top > 1 && role.rawPosition !== position) entries.push({ role: role.id, position });
    }
  } else {
    // Only roles the bot may move are put in the backup's order, using the places those same roles hold now.
    const movable = [];
    for (const r of env.snapshot.roles) {
      const role = findRole(env, r.id);
      if (role && role.id !== guild.id && !role.managed && role.rawPosition < top) movable.push({ r, role });
    }
    const slots = movable.map((m) => m.role.rawPosition).sort((a, b) => a - b);
    movable.sort((a, b) => a.r.position - b.r.position || (a.r.id < b.r.id ? -1 : 1));
    entries = movable.map((m, i) => ({ role: m.role.id, position: slots[i], from: m.role.rawPosition })).filter((e) => e.from !== e.position)
      .map(({ role, position }) => ({ role, position }));
  }
  if (!entries.length) {
    bump(state, 'positionsLimited', limited);
    state.cursor = 1;
    return true;
  }
  const res = await api(env, 'Role order', () => guild.roles.setPositions(entries));
  if (res.ok) bump(state, 'positionsSet', entries.length);
  if (stalled(env)) return false; // a call that timed out is tried again next batch (and counted then)
  bump(state, 'positionsLimited', limited);
  state.cursor = 1; // one attempt per restore: a failing call is reported, not retried for ever
  return true;
}

async function stepChannels(env, list, bucket) {
  const { state } = env;
  // Never a channel without its permission overwrites (it would be open to everyone): no Manage Roles, no channels.
  if (!env.canManageChannels || !env.canManageRoles) {
    if (state.cursor < list.length) {
      bump(state, `${bucket}Skipped`, list.length - state.cursor);
      noteError(state, env.canManageChannels
        ? `I need the Manage Roles permission to give ${bucket} their permissions, so no ${bucket} were restored.`
        : `I need the Manage Channels permission, so no ${bucket} were restored.`);
    }
    state.cursor = list.length;
    return true;
  }
  while (state.cursor < list.length) {
    if (!hasRoom(env)) return false;
    const c = list[state.cursor];
    const d = decideChannel(env, c);
    if (d.action === 'keep') bump(state, `${bucket}Kept`);
    else if (d.action === 'skip') bump(state, `${bucket}Skipped`);
    else if (d.action === 'update') {
      const res = await api(env, `#${c.name}`, () => env.guild.channels.edit(d.existing, d.data));
      if (res.ok) bump(state, `${bucket}Updated`);
    } else {
      const adopted = await adoptChannel(env, c);
      if (adopted) {
        remember(env, c.id, adopted);
        bump(state, `${bucket}Created`);
      } else {
        const res = await api(env, `#${c.name}`, () => env.guild.channels.create(d.data));
        if (res.ok && res.value?.id) {
          remember(env, c.id, res.value);
          bump(state, `${bucket}Created`);
        }
      }
    }
    if (stalled(env)) return false;
    state.cursor += 1;
  }
  return true;
}

async function stepChannelPositions(env) {
  const { state } = env;
  if (env.mode !== 'full' || !env.canManageChannels) return true;
  const entries = [];
  let off = 0;
  for (const c of [...env.snapshot.categories, ...env.snapshot.channels]) {
    const ch = findChannel(env, c.id);
    if (!ch || ch.manageable === false) continue;
    entries.push({ channel: ch.id, position: c.position });
    if (ch.rawPosition !== c.position) off += 1;
  }
  const chunk = RESTORE_BUDGET.POSITION_CHUNK;
  if (state.cursor === 0 && off === 0) return true;
  while (state.cursor * chunk < entries.length) {
    if (!hasRoom(env)) return false;
    const part = entries.slice(state.cursor * chunk, (state.cursor + 1) * chunk);
    const res = await api(env, 'Channel order', () => env.guild.channels.setPositions(part));
    if (res.ok) bump(state, 'positionsSet', part.length);
    if (stalled(env)) return false;
    state.cursor += 1;
  }
  return true;
}

async function stepOverwrites(env) {
  const { state } = env;
  const items = overwriteItems(env.snapshot);
  if (!env.canManageRoles) {
    if (state.cursor < items.length) {
      bump(state, 'overwritesSkipped', items.length - state.cursor);
      if (items.length) noteError(state, 'I need the Manage Roles permission, so no channel permissions were restored.');
    }
    state.cursor = items.length;
    return true;
  }
  while (state.cursor < items.length) {
    if (!hasRoom(env)) return false;
    const item = items[state.cursor];
    const d = decideOverwrite(env, item);
    if (d.action === 'keep') { noteDropped(state, d.dropped ?? 0n); bump(state, 'overwritesKept'); }
    else if (d.action === 'skip') bump(state, 'overwritesSkipped');
    else if (d.action === 'set') await applyOverwrite(env, d);
    if (stalled(env)) return false;
    state.cursor += 1;
  }
  return true;
}

async function applyOverwrite(env, d) {
  const { state, guild } = env;
  if (d.type === 'member') {
    // A member who left cannot be the target of an overwrite.
    let present = guild.members?.cache?.has?.(d.target) === true;
    if (!present) {
      const res = await api(env, 'Member lookup', () => guild.members.fetch(d.target), { quiet: true });
      if (res.timeout) return; // retried in the next batch
      present = res.ok && Boolean(res.value);
    }
    if (!present) return void bump(state, 'overwritesSkipped');
  }
  const body = { type: d.type === 'member' ? OverwriteType.Member : OverwriteType.Role, allow: String(d.allow), deny: String(d.deny) };
  const res = await api(env, `Permissions of #${d.channel.name}`, () => guild.client.rest.put(Routes.channelPermission(d.channel.id, d.target), { body, reason: env.reason }));
  if (res.ok) { noteDropped(state, d.dropped); bump(state, 'overwritesSet'); }
}

const STEPS = Object.freeze({
  roles: stepRoles,
  rolePositions: stepRolePositions,
  categories: (env) => stepChannels(env, env.snapshot.categories, 'categories'),
  channels: (env) => stepChannels(env, env.snapshot.channels, 'channels'),
  channelPositions: stepChannelPositions,
  overwrites: stepOverwrites,
});

/**
 * Runs one bounded batch of the restore (about 10 Discord calls, about 20 seconds at most), advancing `env.state`
 * (`phase`, `cursor`, `idMap`, `made`, `counts`, `errors`). Never throws and never deletes anything.
 * @param {ReturnType<typeof makeEnv>} env
 * @returns {Promise<'more'|'finish'>} 'finish': every phase is done (the caller writes the summary)
 */
export async function runRestoreBatch(env) {
  const { state } = env;
  for (;;) {
    if (state.phase === 'finish') return 'finish';
    const done = await STEPS[state.phase](env);
    if (!done) return 'more';
    state.phase = RESTORE_PHASES[RESTORE_PHASES.indexOf(state.phase) + 1] ?? 'finish';
    state.cursor = 0;
    if (state.phase !== 'finish' && !hasRoom(env)) return 'more';
  }
}
