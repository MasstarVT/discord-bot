// Logging plugin (pkg-logging.md §6, §7): glue between hook contexts, the per-event toggles and ignore list, the pure
// embed builders (logEvents.js) and sendLog (loggingService.js). The thin event files dispatch the hooks; every audit-log
// embed that used to be built inline in them is re-implemented here.
//
// Every hook calls ready() BEFORE building anything, so a disabled event costs no work. The hook runner isolates errors,
// so nothing here needs a defensive try/catch except the best-effort steps (audit-log lookup, settings cleanup,
// bot-managed channel lookup). No side effects at import: the only module-scope state is an empty memo map.
//
// The moderation log (LOG_TYPES.MOD) is never written from here: moderation and automod post their case embeds to
// modLogChannelId themselves (decision D1). Bans, unbans and timeouts are logged to the MEMBER log, so a bot /ban or
// /timeout produces both a case embed (mod log) and a member-log entry: two different channels, by design.
import { AuditLogEvent, PermissionFlagsBits } from 'discord.js';
import prisma from '../database/client.js';
import {
  LOG_EVENTS,
  LOG_TYPES,
  MESSAGE_LOG_EVENTS,
  channelCleanupData,
  forgetDeadChannels,
  isIgnoredChannel,
  isLogEventEnabled,
  markLogChannelDeleted,
  sendLog,
} from '../services/loggingService.js';
import {
  buildAvatarEmbed,
  buildBanEmbed,
  buildBulkDeleteEmbed,
  buildChannelCreateEmbed,
  buildChannelDeleteEmbed,
  buildChannelUpdateEmbed,
  buildEmojiCreateEmbed,
  buildEmojiDeleteEmbed,
  buildEmojiUpdateEmbed,
  buildGuildUpdateEmbed,
  buildInviteCreateEmbed,
  buildInviteDeleteEmbed,
  buildInvitePostedEmbed,
  buildMemberJoinEmbed,
  buildMemberLeaveEmbed,
  buildMemberRolesEmbed,
  buildMessageDeleteEmbed,
  buildMessageEditEmbed,
  buildNicknameEmbed,
  buildRoleCreateEmbed,
  buildRoleDeleteEmbed,
  buildRoleUpdateEmbed,
  buildThreadCreateEmbed,
  buildThreadDeleteEmbed,
  buildThreadUpdateEmbed,
  buildTimeoutEmbed,
  buildUserUpdateEmbed,
  buildVoiceEmbed,
  classifyVoice,
  diffChannel,
  diffEmoji,
  diffGuild,
  diffMemberUpdate,
  diffRole,
  diffThread,
  diffUser,
  findInviteCodes,
} from '../services/logEvents.js';
import { cache } from '../services/redis.js';
import { isModuleEnabled, updateSettingsWith } from '../services/settingsService.js';
import { sleep } from '../utils/async.js';
import { wasDeletedByBot } from '../utils/deletionTracker.js';
import logger from '../utils/logger.js';

export const INVITE_HINT_RE = /discord(?:app)?\.com\/invite\/|discord\.gg\//i;

const TIMEOUT_KEY_MIN_TTL_S = 60;
const TIMEOUT_KEY_MAX_TTL_S = 2_419_200; // 28 days, Discord's longest timeout
const MANAGED_MEMO_TTL_MS = 10 * 60_000;
const MANAGED_MEMO_MAX = 2_000;
const AUDIT_LOG_DELAY_MS = 1_500;
const AUDIT_LOOKUPS_PER_WINDOW = 5;
const AUDIT_WINDOW_MS = 10_000;

// ── Hook context helpers (§6.2) ───────────────────────────────────────────────

/**
 * The settings when the event should be logged, otherwise null: settings loaded, the event not disabled and, for
 * message events, the channel not ignored.
 */
async function ready(ctx, event, channel = null) {
  const settings = await ctx.getSettings(); // memoised; already loaded by the module gate
  if (!settings || !isLogEventEnabled(settings, event)) return null;
  if (channel && MESSAGE_LOG_EVENTS.includes(event) && isIgnoredChannel(settings, channel)) return null;
  return settings;
}

async function emit(ctx, event, embed, extra = {}) {
  if (!embed) return;
  await sendLog(ctx.guild, LOG_EVENTS[event].type, embed, { event, settings: await ctx.getSettings(), ...extra });
}

const isLogChannelOf = (settings, channelId) => Object.values(LOG_TYPES).some((field) => settings?.[field] === channelId);

// ── Messages (§6.3) ───────────────────────────────────────────────────────────

async function onMessageDelete(ctx) {
  const { message } = ctx;
  // Bot-authored panels (giveaways, polls, starboard posts) are never logged.
  if (message.author?.bot || message.webhookId) return;
  const channel = message.channel ?? ctx.guild.channels.cache.get(message.channelId);
  if (!(await ready(ctx, 'messageDelete', channel))) return;
  const botReason = wasDeletedByBot(message.id);
  if (message.partial && botReason) return; // nothing useful is known about it
  await emit(ctx, 'messageDelete', buildMessageDeleteEmbed({ message, botReason, now: Date.now() }));
}

async function onMessageEdit(ctx) {
  const { oldMessage, newMessage, oldKnown } = ctx;
  // Core already dropped non-edits (R120/R121); these are the defensive repeats.
  if (!oldKnown && !newMessage.editedTimestamp) return;
  if (oldKnown && oldMessage.content === newMessage.content) return;
  if (!(await ready(ctx, 'messageEdit', newMessage.channel))) return;
  await emit(ctx, 'messageEdit', buildMessageEditEmbed({ oldMessage, newMessage, oldKnown, now: Date.now() }));
}

async function onBulkDelete(ctx) {
  const { messages, channel } = ctx;
  if (!(await ready(ctx, 'messageBulkDelete', channel))) return;
  let botReason = null;
  for (const id of messages.keys()) {
    botReason = wasDeletedByBot(id);
    if (botReason) break;
  }
  const { embed, file } = buildBulkDeleteEmbed({ channel, messages, botReason });
  await emit(ctx, 'messageBulkDelete', embed, { files: file ? [file] : [] });
}

async function onInvitePosted(ctx) {
  const { message } = ctx;
  if (!(await ready(ctx, 'invitePosted', message.channel))) return;
  const codes = findInviteCodes(message.content);
  if (!codes.length) return;
  await emit(ctx, 'invitePosted', buildInvitePostedEmbed({ message, codes, vanityCode: ctx.guild.vanityURLCode }));
}

// ── Members (§6.4) ────────────────────────────────────────────────────────────

/** guildId -> { start, count }: audit-log lookups in the current window (a mass ban must not flood the REST queue). */
const auditBudget = new Map();

function takeAuditLookup(guildId) {
  const now = Date.now();
  let b = auditBudget.get(guildId);
  if (!b || now - b.start >= AUDIT_WINDOW_MS) {
    for (const [id, x] of auditBudget) if (now - x.start >= AUDIT_WINDOW_MS) auditBudget.delete(id);
    b = { start: now, count: 0 };
    auditBudget.set(guildId, b);
  }
  if (b.count >= AUDIT_LOOKUPS_PER_WINDOW) return false;
  b.count++;
  return true;
}

async function onMemberJoin(ctx) {
  if (!(await ready(ctx, 'memberJoin'))) return;
  // ctx.state.invite: { code, inviterId, vanity } | null (unknown) | undefined (the invites module recorded
  // nothing). Passed through untouched: the builder omits the field for undefined and says Unknown for null.
  const now = Date.now();
  const send = (invite) => emit(ctx, 'memberJoin', buildMemberJoinEmbed({ member: ctx.member, invite, now }));
  const pending = ctx.state.invitePromise;
  if (!pending) return send(ctx.state.invite);
  // Attribution is still running (see the invites plugin): post the log when it settles instead of holding up the
  // captcha and welcome hooks that run after this one. invitePromise never rejects.
  void pending.then(send).catch((err) => logger.error('logging: member join log failed', err));
  return undefined;
}

async function onMemberLeave(ctx) {
  if (!ctx.member?.user) return;
  if (!(await ready(ctx, 'memberLeave'))) return;
  await emit(ctx, 'memberLeave', buildMemberLeaveEmbed({ member: ctx.member, now: Date.now() }));
}

/**
 * Whether a timeout entry should be logged. `added` and `changed` always log; the dedupe key is only recorded so
 * that a later partial update (previous state unknown) does not log the same timeout again. For `active-unknown`
 * Redis down (setNX -> null) FAILS OPEN: the entry is logged and may appear twice, which beats losing it.
 */
async function timeoutIsFresh(ctx, member, timeout) {
  if (timeout.kind === 'removed') return true;
  const untilMs = timeout.until instanceof Date ? timeout.until.getTime() : Number(timeout.until);
  if (!Number.isFinite(untilMs)) return true;
  const untilSec = Math.floor(untilMs / 1000);
  const ttl = Math.min(TIMEOUT_KEY_MAX_TTL_S, Math.max(TIMEOUT_KEY_MIN_TTL_S, untilSec - Math.floor(Date.now() / 1000)));
  const fresh = await cache.setNX(`log:timeout:${ctx.guild.id}:${member.id}:${untilSec}`, 1, ttl);
  if (timeout.kind === 'active-unknown') return fresh !== false;
  return true;
}

async function onMemberUpdate(ctx) {
  const { oldMember, newMember, oldKnown } = ctx;
  // Housekeeping first, whatever the toggles say: the bot's own roles may have changed, so a dead log channel
  // may be usable again.
  if (newMember.id === ctx.client?.user?.id) forgetDeadChannels(ctx.guild.id);

  const settings = await ctx.getSettings();
  if (!settings) return;
  const d = diffMemberUpdate({ oldMember, newMember, oldKnown, now: Date.now() });

  // One embed per changed part, in this order (MEE6 logs each separately).
  if (d.roles && isLogEventEnabled(settings, 'memberRoles')) {
    await emit(ctx, 'memberRoles', buildMemberRolesEmbed({ member: newMember, roles: d.roles }));
  }
  if (d.nickname && isLogEventEnabled(settings, 'memberNickname')) {
    await emit(ctx, 'memberNickname', buildNicknameEmbed({ member: newMember, nickname: d.nickname }));
  }
  if (d.timeout && isLogEventEnabled(settings, 'memberTimeout') && await timeoutIsFresh(ctx, newMember, d.timeout)) {
    await emit(ctx, 'memberTimeout', buildTimeoutEmbed({ member: newMember, timeout: d.timeout }));
  }
  if (d.avatar && isLogEventEnabled(settings, 'memberAvatar')) {
    await emit(ctx, 'memberAvatar', buildAvatarEmbed({ member: newMember, avatar: d.avatar }));
  }
}

async function onUserUpdate(ctx) {
  const { oldUser, newUser } = ctx;
  if (!(await ready(ctx, 'userUpdate'))) return;
  const diff = diffUser(oldUser, newUser);
  await emit(ctx, 'userUpdate', buildUserUpdateEmbed({ user: newUser, diff }));
}

async function onBan(ctx, unban) {
  const event = unban ? 'memberUnban' : 'memberBan';
  if (!(await ready(ctx, event))) return;
  const { user } = ctx;

  // Executor lookup: best-effort, one REST call per ban (bans are rare). Discord writes the audit entry a moment
  // after the gateway event.
  // At most 5 lookups per guild per 10 s: during a mass ban the rest are logged with an unknown executor.
  let entry = null;
  if (ctx.guild.members.me?.permissions.has(PermissionFlagsBits.ViewAuditLog) && takeAuditLookup(ctx.guild.id)) {
    await sleep(AUDIT_LOG_DELAY_MS);
    const logs = await ctx.guild
      .fetchAuditLogs({ type: unban ? AuditLogEvent.MemberBanRemove : AuditLogEvent.MemberBanAdd, limit: 5 })
      .catch(() => null);
    entry = logs?.entries.find((e) => e.targetId === user.id && Date.now() - e.createdTimestamp < 60_000) ?? null;
  }
  await emit(ctx, event, buildBanEmbed({
    user,
    executorId: entry?.executorId ?? null,
    reason: entry?.reason ?? ctx.ban?.reason ?? null,
    unban,
    botId: ctx.client.user.id,
  }));
}

// ── Voice (§6.5) ──────────────────────────────────────────────────────────────

const VOICE_EVENT = { join: 'voiceJoin', leave: 'voiceLeave', move: 'voiceMove', state: 'voiceState' };

async function onVoiceState(ctx) {
  const { oldState, newState, member } = ctx;
  // Classified by channel ids, not by channel objects: a deleted channel still has its id (R269).
  const c = classifyVoice(oldState, newState);
  if (!c.kind) return;
  const event = VOICE_EVENT[c.kind];
  if (!(await ready(ctx, event))) return;
  await emit(ctx, event, buildVoiceEmbed({ member, guild: ctx.guild, c }));
}

// ── Channels (§6.6) ───────────────────────────────────────────────────────────

/** Channel id -> { value, until }: whether a StatChannel / TempVoiceChannel row owns the channel. */
const managedMemo = new Map();

/**
 * Read-only lookup of the bot-managed channel tables, memoised per channel for 10 minutes. Fails open: when the
 * query fails the channel counts as not managed (the rename is logged).
 */
async function isBotManagedChannel(channelId) {
  const hit = managedMemo.get(channelId);
  if (hit && hit.until > Date.now()) return hit.value;
  let value;
  try {
    const [stat, temp] = await Promise.all([
      prisma.statChannel.findUnique({ where: { channelId }, select: { id: true } }),
      prisma.tempVoiceChannel.findUnique({ where: { channelId }, select: { id: true } }),
    ]);
    value = Boolean(stat || temp);
  } catch (err) {
    logger.debug(`logging: bot-managed channel lookup failed: ${err?.message ?? err}`);
    return false;
  }
  managedMemo.delete(channelId);
  managedMemo.set(channelId, { value, until: Date.now() + MANAGED_MEMO_TTL_MS });
  while (managedMemo.size > MANAGED_MEMO_MAX) managedMemo.delete(managedMemo.keys().next().value);
  return value;
}

async function onChannelCreate(ctx) {
  if (!(await ready(ctx, 'channelCreate'))) return;
  await emit(ctx, 'channelCreate', buildChannelCreateEmbed({ channel: ctx.channel ?? ctx.newChannel }));
}

async function onChannelUpdate(ctx) {
  const { oldChannel, newChannel } = ctx;
  // Housekeeping first: a log channel that changed may be usable again.
  const settings = await ctx.getSettings();
  if (settings && isLogChannelOf(settings, newChannel.id)) forgetDeadChannels(ctx.guild.id, newChannel.id);
  if (!(await ready(ctx, 'channelUpdate'))) return;

  const lines = diffChannel(oldChannel, newChannel); // position-only changes give no lines
  if (!lines.length) return;
  // Stat and temp-voice channels are renamed by the bot all the time: skip name-only entries of those.
  if (lines.length === 1 && lines[0].name === 'Name' && await isBotManagedChannel(newChannel.id)) return;
  await emit(ctx, 'channelUpdate', buildChannelUpdateEmbed({ channel: newChannel, lines }));
}

// ALWAYS (declared with always: true): the settings cleanup must run even while the module is off.
async function onChannelDelete(ctx) {
  const { channel, guild } = ctx;
  const settings = await ctx.getSettings();
  if (!settings) return;

  // A deleted log channel: later sendLog calls return at once, without a REST call.
  if (isLogChannelOf(settings, channel.id)) markLogChannelDeleted(guild.id, channel.id);

  if (isModuleEnabled(settings, 'logging') && isLogEventEnabled(settings, 'channelDelete')) {
    // Best-effort (R269): who was connected when a voice channel was deleted.
    let occupants = [];
    if (channel.isVoiceBased?.()) {
      occupants = [...guild.voiceStates.cache.values()].filter((vs) => vs.channelId === channel.id).map((vs) => vs.id);
    }
    await emit(ctx, 'channelDelete', buildChannelDeleteEmbed({ channel, occupants, botReason: wasDeletedByBot(channel.id) }));
  }

  // Cleanup (R147): forget the deleted channel in every log field (the moderation log included) and the ignore list.
  if (channelCleanupData(settings, channel.id)) {
    try {
      await updateSettingsWith(guild.id, (cur) => channelCleanupData(cur, channel.id));
      logger.info(`Cleared deleted log channel ${channel.id} from guild ${guild.id} settings`);
    } catch (err) {
      logger.warn(`Could not clear deleted log channel ${channel.id} from guild ${guild.id} settings: ${err?.message ?? err}`);
    }
  }
}

// ── Threads ───────────────────────────────────────────────────────────────────
// Thread events are not message events: the ignore list does not apply. Archive / unarchive is not logged
// (diffThread), so a thread that archives itself after inactivity stays quiet.

async function onThreadCreate(ctx) {
  if (!(await ready(ctx, 'threadCreate'))) return;
  await emit(ctx, 'threadCreate', buildThreadCreateEmbed({ thread: ctx.thread }));
}

async function onThreadUpdate(ctx) {
  const { oldThread, newThread } = ctx;
  if (!(await ready(ctx, 'threadUpdate'))) return;
  const lines = diffThread(oldThread, newThread); // archive changes and the like give no lines
  if (!lines.length) return;
  await emit(ctx, 'threadUpdate', buildThreadUpdateEmbed({ thread: newThread, lines }));
}

async function onThreadDelete(ctx) {
  const { thread } = ctx;
  if (!(await ready(ctx, 'threadDelete'))) return;
  await emit(ctx, 'threadDelete', buildThreadDeleteEmbed({ thread, botReason: wasDeletedByBot(thread.id) }));
}

// ── Roles (§6.7) ──────────────────────────────────────────────────────────────

async function onRoleCreate(ctx) {
  if (!(await ready(ctx, 'roleCreate'))) return;
  await emit(ctx, 'roleCreate', buildRoleCreateEmbed({ role: ctx.role }));
}

async function onRoleUpdate(ctx) {
  const { oldRole, newRole, guild } = ctx;
  // The bot's own roles (or @everyone) changed: a dead log channel may be usable again.
  if (guild.members.me?.roles.cache.has(newRole.id) || newRole.id === guild.id) forgetDeadChannels(guild.id);
  if (!(await ready(ctx, 'roleUpdate'))) return;
  const lines = diffRole(oldRole, newRole); // position-only changes give no lines
  await emit(ctx, 'roleUpdate', buildRoleUpdateEmbed({ role: newRole, lines }));
}

async function onRoleDelete(ctx) {
  if (!(await ready(ctx, 'roleDelete'))) return;
  await emit(ctx, 'roleDelete', buildRoleDeleteEmbed({ role: ctx.role }));
}

// ── Server, emojis, invites (§6.8-§6.10) ──────────────────────────────────────

async function onGuildUpdate(ctx) {
  if (!(await ready(ctx, 'serverUpdate'))) return;
  const { oldGuild, newGuild } = ctx;
  await emit(ctx, 'serverUpdate', buildGuildUpdateEmbed({ oldGuild, newGuild, lines: diffGuild(oldGuild, newGuild) }));
}

async function onEmojiCreate(ctx) {
  if (!(await ready(ctx, 'emojiCreate'))) return;
  await emit(ctx, 'emojiCreate', buildEmojiCreateEmbed({ emoji: ctx.emoji }));
}

async function onEmojiUpdate(ctx) {
  if (!(await ready(ctx, 'emojiUpdate'))) return;
  const { oldEmoji, newEmoji } = ctx;
  await emit(ctx, 'emojiUpdate', buildEmojiUpdateEmbed({ emoji: newEmoji, lines: diffEmoji(oldEmoji, newEmoji) }));
}

async function onEmojiDelete(ctx) {
  if (!(await ready(ctx, 'emojiDelete'))) return;
  await emit(ctx, 'emojiDelete', buildEmojiDeleteEmbed({ emoji: ctx.emoji }));
}

async function onInviteCreate(ctx) {
  if (!(await ready(ctx, 'inviteCreate'))) return;
  await emit(ctx, 'inviteCreate', buildInviteCreateEmbed({ invite: ctx.invite }));
}

async function onInviteDelete(ctx) {
  if (!(await ready(ctx, 'inviteDelete'))) return;
  await emit(ctx, 'inviteDelete', buildInviteDeleteEmbed({ invite: ctx.invite }));
}

// ── Declaration (§7) ──────────────────────────────────────────────────────────
// Every hook id is the literal 'logging'. All hooks are module-gated (plugin.module) except channelDelete.

export const plugin = {
  id: 'logging',
  module: 'logging',
  hooks: [
    { event: 'messageCreate',     id: 'logging', filter: (ctx) => typeof ctx.message?.content === 'string' && INVITE_HINT_RE.test(ctx.message.content), run: onInvitePosted },
    { event: 'messageUpdate',     id: 'logging', run: onMessageEdit },
    { event: 'messageDelete',     id: 'logging', run: onMessageDelete },
    { event: 'messageDeleteBulk', id: 'logging', run: onBulkDelete, timeoutMs: 15_000 },
    { event: 'voiceStateUpdate',  id: 'logging', run: onVoiceState },
    { event: 'guildMemberAdd',    id: 'logging', run: onMemberJoin },
    { event: 'guildMemberRemove', id: 'logging', run: onMemberLeave },
    { event: 'guildMemberUpdate', id: 'logging', run: onMemberUpdate },
    { event: 'userUpdate',        id: 'logging', run: onUserUpdate },
    { event: 'channelCreate',     id: 'logging', run: onChannelCreate },
    { event: 'channelUpdate',     id: 'logging', run: onChannelUpdate },
    { event: 'channelDelete',     id: 'logging', run: onChannelDelete, always: true },
    { event: 'threadCreate',      id: 'logging', run: onThreadCreate },
    { event: 'threadUpdate',      id: 'logging', run: onThreadUpdate },
    { event: 'threadDelete',      id: 'logging', run: onThreadDelete },
    { event: 'roleCreate',        id: 'logging', run: onRoleCreate },
    { event: 'roleUpdate',        id: 'logging', run: onRoleUpdate },
    { event: 'roleDelete',        id: 'logging', run: onRoleDelete },
    { event: 'guildBanAdd',       id: 'logging', run: (ctx) => onBan(ctx, false) },
    { event: 'guildBanRemove',    id: 'logging', run: (ctx) => onBan(ctx, true) },
    { event: 'guildUpdate',       id: 'logging', run: onGuildUpdate },
    { event: 'emojiCreate',       id: 'logging', run: onEmojiCreate },
    { event: 'emojiUpdate',       id: 'logging', run: onEmojiUpdate },
    { event: 'emojiDelete',       id: 'logging', run: onEmojiDelete },
    { event: 'inviteCreate',      id: 'logging', run: onInviteCreate },
    { event: 'inviteDelete',      id: 'logging', run: onInviteDelete },
  ],
  components: [],
  jobs: [],
  provides: {},
};
