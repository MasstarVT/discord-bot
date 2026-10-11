// Pure embed builders and diff functions for every audit-log event (pkg-logging.md §6). No I/O, no settings, no
// Discord REST, no Redis: everything here takes plain objects (discord.js classes or test fakes) and returns an
// EmbedBuilder (or null when there is nothing to log), or an array of { name, value } diff lines.
//
// Conventions (§6.0): every embed comes from the neutralEmbed factory, every user-influenced string is clamped,
// every list goes through joinWithLimit (never a raw slice, R260), and fitEmbed runs last in every builder.
import {
  AttachmentBuilder,
  GuildDefaultMessageNotifications,
  GuildExplicitContentFilter,
  GuildMFALevel,
  GuildNSFWLevel,
  GuildVerificationLevel,
} from 'discord.js';
import { COLORS } from '../config/constants.js';
import { neutralEmbed } from '../utils/embedBuilder.js';
import { permissionNames } from '../utils/interactionHelpers.js';
import { formatDurationLong } from '../utils/parseDuration.js';
import { clampText, formatNumber, joinWithLimit } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { channelTypeLabel, extractMentions, fitEmbedData } from './loggingService.js';

export const LOG_COLORS = Object.freeze({
  CREATE:  COLORS.SUCCESS,
  DELETE:  COLORS.ERROR,
  EDIT:    COLORS.WARNING,
  UPDATE:  COLORS.INFO,
  ALERT:   0xff9800,
  NEUTRAL: COLORS.NEUTRAL,
});

const HOUR_MS = 3_600_000;
const DAY_MS = 86_400_000;
const NEW_ACCOUNT_MS = 7 * DAY_MS;
const TRANSCRIPT_MAX_CHARS = 1_000_000;
const NO_TEXT = '*[No text content]*';

// ── Shared helpers ────────────────────────────────────────────────────────────

/** An embed from the factory with an event colour. */
export function logEmbed(color, title) {
  return neutralEmbed(title).setColor(color);
}

/**
 * Clamps an embed to Discord's limits, in place: at most 25 fields (24 plus "… and N more changes"), every
 * string cut, and at most 6000 characters in total. Returns the same embed.
 * @param {import('discord.js').EmbedBuilder} embed
 * @returns {import('discord.js').EmbedBuilder}
 */
export function fitEmbed(embed) {
  const fitted = fitEmbedData(embed.toJSON());
  embed.data.description = fitted.description;
  if (fitted.description === undefined) delete embed.data.description;
  embed.data.fields = fitted.fields;
  if (fitted.fields === undefined) delete embed.data.fields;
  return embed;
}

const field = (name, value, inline = false) => ({ name: clampText(name, 256) || '​', value: clampText(value, 1024) || '​', inline });
const setFields = (embed, fields) => {
  const list = fields.filter(Boolean);
  if (list.length) embed.data.fields = list;
  return embed;
};
const setDescription = (embed, text, max = 4096) => (text ? embed.setDescription(clampText(text, max)) : embed);
const setFooter = (embed, text) => embed.setFooter({ text: clampText(text, 2048) });
const setThumb = (embed, url) => {
  if (!url) return embed;
  try { embed.setThumbnail(url); } catch { /* an invalid URL is simply not shown */ }
  return embed;
};

export const userTag = (user) => clampText(user?.tag ?? user?.username ?? user?.id ?? 'Unknown', 100);
export const userLine = (user) => (user?.id ? `<@${user.id}> \`${userTag(user).replace(/`/g, "'")}\`` : 'Unknown');
const avatarOf = (user) => {
  try { return user?.displayAvatarURL?.() ?? null; } catch { return null; }
};
const setAuthor = (embed, user) => {
  if (!user?.id) return embed;
  const icon = avatarOf(user);
  try {
    embed.setAuthor(icon ? { name: userTag(user), iconURL: icon } : { name: userTag(user) });
  } catch {
    embed.setAuthor({ name: userTag(user) });
  }
  return embed;
};

/** `<#id>` plus " (deleted channel)" when the guild's channel cache is known and lacks it. */
export function channelRef(guild, id) {
  if (!id) return '*(none)*';
  const cache = guild?.channels?.cache;
  return `<#${id}>${cache && typeof cache.has === 'function' && !cache.has(id) ? ' (deleted channel)' : ''}`;
}

const roleRef = (id) => `<@&${id}>`;
const yesNo = (v) => (v ? 'Yes' : 'No');
const values = (collection) => (collection ? [...(typeof collection.values === 'function' ? collection.values() : collection)] : []);
const keys = (collection) => (collection ? [...(typeof collection.keys === 'function' ? collection.keys() : [])] : []);

/** Inline code for a changed value; backticks inside are replaced, empty becomes "(none)". */
const code = (v, max = 200) => {
  if (v === null || v === undefined || v === '') return '*(none)*';
  return `\`${clampText(String(v).replace(/`/g, "'"), max)}\``;
};
const change = (before, after) => `${code(before)} → ${code(after)}`;

const sameSet = (a, b) => a.length === b.length && a.every((x) => b.includes(x));

/** `bigint` of a PermissionsBitField, bigint, number or null. */
const bits = (x) => {
  if (typeof x === 'bigint') return x;
  if (x && typeof x.bitfield === 'bigint') return x.bitfield;
  try { return BigInt(x ?? 0); } catch { return 0n; }
};

function formatBytes(n) {
  const v = Number(n);
  if (!Number.isFinite(v) || v < 0) return '?';
  if (v < 1024) return `${v} B`;
  if (v < 1024 * 1024) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / (1024 * 1024)).toFixed(1)} MB`;
}

/** HEAD's formatter, verbatim. */
export function formatTimeInServer(ms) {
  const d = Math.floor(ms / 86_400_000);
  const h = Math.floor((ms % 86_400_000) / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m`;
  return 'Just joined';
}

const msgFooter = (message) => (message.author?.id
  ? `User ID: ${message.author.id} · Msg ID: ${message.id}`
  : `Msg ID: ${message.id}`);

// ── Messages (§6.3) ───────────────────────────────────────────────────────────

/**
 * Ghost-ping rule (R118/R267): a message is a ghost ping when it mentioned someone other than its author
 * (and not a bot) and was removed within an hour, and the bot did not delete it itself.
 * @param {{ message: object, botReason?: string|null, now?: number }} p
 * @returns {{ ghost: boolean, mentions: string[] }}
 */
export function isGhostPing({ message, botReason = null, now = Date.now() }) {
  const none = { ghost: false, mentions: [] };
  if (!message || message.partial || botReason) return none;
  const created = message.createdTimestamp;
  if (!Number.isFinite(created) || now - created > HOUR_MS) return none;
  const mentions = extractMentions(message, { excludeUserId: message.author?.id });
  return { ghost: mentions.length > 0, mentions };
}

/**
 * @param {{ message: object, botReason?: string|null, now?: number }} p
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildMessageDeleteEmbed({ message, botReason = null, now = Date.now() }) {
  const ghost = isGhostPing({ message, botReason, now });
  let title = '🗑️ Message Deleted';
  let color = LOG_COLORS.DELETE;
  if (ghost.ghost) {
    title = '👻 Ghost Ping Detected — Message Deleted';
    color = LOG_COLORS.ALERT;
  } else if (botReason) {
    title = `🗑️ Message Deleted (removed by the bot: ${clampText(botReason, 100)})`;
  }
  const embed = logEmbed(color, title);
  const channelId = message.channelId ?? message.channel?.id;
  const fields = [];

  if (message.author) {
    setAuthor(embed, message.author);
    fields.push(field('Author', userLine(message.author), true));
  }
  fields.push(field('Channel', channelRef(message.guild, channelId), true));

  if (message.partial) setDescription(embed, '*Message was not cached — content unavailable.*');
  else if (message.content) setDescription(embed, clampText(message.content, 4000));
  else setDescription(embed, NO_TEXT);

  if (message.attachments?.size) {
    const list = values(message.attachments).map((a) => `${clampText(a.name ?? 'file', 100)} (${formatBytes(a.size)})`);
    fields.push(field(`Attachments (${message.attachments.size})`, joinWithLimit(list, { sep: '\n', max: 1024 })));
  }
  if (message.embeds?.length && !message.content) fields.push(field('Embeds', `${message.embeds.length} embed(s)`));
  if (ghost.ghost) fields.push(field('⚠️ Ghost Pinged', joinWithLimit(ghost.mentions, { sep: '\n', max: 1024 })));
  if (message.createdAt) fields.push(field('Sent', discordTimestamp(message.createdAt, 'R'), true));

  setFooter(embed, msgFooter(message));
  setFields(embed, fields);
  return fitEmbed(embed);
}

/**
 * @param {{ oldMessage: object, newMessage: object, oldKnown: boolean, now?: number }} p
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildMessageEditEmbed({ oldMessage, newMessage, oldKnown, now = Date.now() }) {
  const author = newMessage.author;
  let removed = [];
  let ghost = false;
  if (oldKnown) {
    const before = extractMentions(oldMessage, { excludeUserId: author?.id });
    const after = new Set(extractMentions(newMessage, { excludeUserId: author?.id }));
    removed = before.filter((m) => !after.has(m));
    ghost = removed.length > 0 && now - newMessage.createdTimestamp <= HOUR_MS;
  }
  const embed = logEmbed(ghost ? LOG_COLORS.ALERT : LOG_COLORS.EDIT, ghost ? '👻 Ghost Ping Detected — Message Edited' : '✏️ Message Edited');
  embed.setTimestamp(newMessage.editedTimestamp ?? now);
  setAuthor(embed, author);

  const fields = [];
  if (author) fields.push(field('Author', userLine(author), true));
  fields.push(field('Channel', channelRef(newMessage.guild, newMessage.channelId ?? newMessage.channel?.id), true));
  // R268: a cached message with empty content is not "not cached".
  const beforeText = oldKnown
    ? (oldMessage.content ? clampText(oldMessage.content, 1024) : NO_TEXT)
    : '*Not cached*';
  fields.push(field('Before', beforeText));
  fields.push(field('After', newMessage.content ? clampText(newMessage.content, 1024) : NO_TEXT));
  if (ghost) fields.push(field('⚠️ Removed Mentions', joinWithLimit(removed, { sep: '\n', max: 1024 })));
  if (newMessage.url) fields.push(field('Jump', `[View Message](${newMessage.url})`, true));

  setFooter(embed, msgFooter(newMessage));
  setFields(embed, fields);
  return fitEmbed(embed);
}

/**
 * Plain-text transcript of a bulk delete: oldest first, unknown times last, capped at 1,000,000 characters.
 * @param {Iterable<object>|Map<string, object>} messages
 * @param {{ name?: string }|null} [channel]
 * @returns {string}
 */
export function buildBulkDeleteTranscript(messages, channel = null) {
  const list = values(messages);
  const rank = (m) => (Number.isFinite(m.createdTimestamp) ? m.createdTimestamp : Infinity);
  const sorted = [...list].sort((a, b) => {
    const ra = rank(a);
    const rb = rank(b);
    if (ra === rb) return 0;
    return ra < rb ? -1 : 1;
  });

  const out = [`Bulk delete — ${list.length} messages — channel ${channel?.name ?? ''}`];
  let length = out[0].length;
  let written = 0;
  for (const m of sorted) {
    let line;
    if (m.partial || !m.author) {
      line = `[unknown time] (not cached) message ${m.id}`;
    } else {
      const ts = Number.isFinite(m.createdTimestamp) ? new Date(m.createdTimestamp).toISOString() : 'unknown time';
      const content = String(m.content ?? '').replace(/\r?\n/g, ' ⏎ ');
      const urls = values(m.attachments).map((a) => a.url).filter(Boolean);
      line = `[${ts}] ${userTag(m.author)} (${m.author.id})${m.author.bot ? ' [BOT]' : ''}: ${content}${urls.length ? ` [attachments: ${urls.join(' ')}]` : ''}`;
    }
    if (length + 1 + line.length > TRANSCRIPT_MAX_CHARS) break;
    out.push(line);
    length += 1 + line.length;
    written++;
  }
  if (written < sorted.length) out.push(`… transcript truncated (${sorted.length - written} more messages)`);
  return out.join('\n');
}

/**
 * One embed for a bulk delete plus a transcript file (null when no message was cached).
 * @param {{ channel: object, messages: Iterable<object>|Map<string, object>, botReason?: string|null }} p
 * @returns {{ embed: import('discord.js').EmbedBuilder, file: AttachmentBuilder|null }}
 */
export function buildBulkDeleteEmbed({ channel, messages, botReason = null }) {
  const list = values(messages);
  const n = list.length;
  const cached = list.filter((m) => !m.partial && m.author);
  const embed = logEmbed(LOG_COLORS.DELETE, '🧹 Messages Bulk-Deleted');
  setDescription(embed, `**${n}** messages were deleted in ${channelRef(channel?.guild, channel?.id)}${botReason ? ` (by the bot: ${clampText(botReason, 100)})` : ''}.`);

  const fields = [field('Cached', `${cached.length} of ${n} messages had cached content.`)];
  if (cached.length) {
    const counts = new Map();
    for (const m of cached) counts.set(m.author.id, (counts.get(m.author.id) ?? 0) + 1);
    const authors = [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([id, c]) => `<@${id}> ×${c}`);
    fields.push(field('Authors', joinWithLimit(authors, { sep: ', ', max: 1024 })));
  }

  let file = null;
  if (cached.length > 0) {
    const text = buildBulkDeleteTranscript(list, channel);
    file = new AttachmentBuilder(Buffer.from(text, 'utf8'), { name: `bulk-delete-${channel?.id ?? 'channel'}-${Date.now()}.txt` });
    fields.push(field('Transcript', 'Attached as a text file.'));
  }
  setFooter(embed, `Channel ID: ${channel?.id ?? 'unknown'}`);
  setFields(embed, fields);
  return { embed: fitEmbed(embed), file };
}

// ── Invite posted (§6.3.5) ────────────────────────────────────────────────────

const INVITE_CODE_RE = /(?:https?:\/\/)?(?:www\.)?(?:discord(?:app)?\.com\/invite|discord\.gg)\/([A-Za-z0-9-]{2,32})/g;

/**
 * The unique invite codes in a message, in order of appearance, at most 10.
 * @param {string} content
 * @returns {string[]}
 */
export function findInviteCodes(content) {
  if (typeof content !== 'string') return [];
  const codes = [];
  for (const m of content.slice(0, 4000).matchAll(INVITE_CODE_RE)) {
    if (!codes.includes(m[1])) codes.push(m[1]);
    if (codes.length >= 10) break;
  }
  return codes;
}

/**
 * @param {{ message: object, codes: string[], vanityCode?: string|null }} p
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildInvitePostedEmbed({ message, codes, vanityCode = null }) {
  const embed = logEmbed(LOG_COLORS.ALERT, '📨 Invite Posted');
  setAuthor(embed, message.author);
  const vanity = vanityCode ? String(vanityCode).toLowerCase() : null;
  const fields = [];
  if (message.author) fields.push(field('Author', userLine(message.author), true));
  fields.push(field('Channel', channelRef(message.guild, message.channelId ?? message.channel?.id), true));
  fields.push(field('Invites', joinWithLimit(
    codes.map((c) => `discord.gg/${c}${vanity && c.toLowerCase() === vanity ? " (this server's vanity URL)" : ''}`),
    { sep: '\n', max: 1024 },
  )));
  if (message.url) fields.push(field('Jump', `[View Message](${message.url})`, true));
  setDescription(embed, clampText(message.content, 4000));
  setFooter(embed, msgFooter(message));
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Joins and leaves (§6.4.1, §6.4.2) ─────────────────────────────────────────

/**
 * `invite` is `undefined` when the invites module has not recorded anything (the field is omitted), `null` when
 * it could not tell (Unknown), or `{ code, inviterId, vanity }`.
 * @param {{ member: object, invite?: { code?: string, inviterId?: string|null, vanity?: boolean }|null, now?: number }} p
 */
export function buildMemberJoinEmbed({ member, invite, now = Date.now() }) {
  const user = member.user;
  const isBot = Boolean(user?.bot);
  const isNew = Number.isFinite(user?.createdTimestamp) && now - user.createdTimestamp < NEW_ACCOUNT_MS;
  let title;
  let color;
  if (isBot) { title = '🤖 Bot Added'; color = LOG_COLORS.UPDATE; }
  else if (isNew) { title = '⚠️ New Account Joined'; color = LOG_COLORS.ALERT; }
  else { title = '📥 Member Joined'; color = LOG_COLORS.CREATE; }

  const embed = logEmbed(color, title);
  setThumb(embed, avatarOf(user));
  const fields = [field('User', userLine(user), true)];
  if (user?.createdAt) fields.push(field('Account Created', discordTimestamp(user.createdAt, 'R'), true));
  fields.push(field('Member Count', `#${formatNumber(member.guild?.memberCount)}`, true));
  if (invite === null) {
    fields.push(field('Invite', 'Unknown', true));
  } else if (invite && typeof invite === 'object') {
    if (invite.vanity) fields.push(field('Invite', 'Vanity URL', true));
    else if (invite.code) fields.push(field('Invite', `discord.gg/${invite.code}${invite.inviterId ? ` by <@${invite.inviterId}>` : ''}`, true));
    else fields.push(field('Invite', 'Unknown', true));
  }
  if (isNew && !isBot) setDescription(embed, '⚠️ **This account was created less than 7 days ago.**');
  setFooter(embed, `User ID: ${user?.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/** @param {{ member: object, now?: number }} p */
export function buildMemberLeaveEmbed({ member, now = Date.now() }) {
  const user = member.user;
  const embed = logEmbed(LOG_COLORS.DELETE, '📤 Member Left');
  setThumb(embed, avatarOf(user));
  const fields = [
    field('User', userLine(user), true),
    field('Time in Server', member.joinedTimestamp ? formatTimeInServer(now - member.joinedTimestamp) : 'Unknown', true),
  ];
  if (!member.partial) {
    const guildId = member.guild?.id;
    const roles = values(member.roles?.cache)
      .filter((r) => r.id !== guildId)
      .sort((a, b) => (b.position ?? 0) - (a.position ?? 0))
      .map((r) => roleRef(r.id));
    if (roles.length) fields.push(field('Roles', joinWithLimit(roles, { sep: ', ', max: 1024 })));
  }
  setFooter(embed, `User ID: ${user?.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Member updates (§6.4.3) ───────────────────────────────────────────────────

const untilOf = (member) => {
  if (member.communicationDisabledUntil) return member.communicationDisabledUntil;
  const ts = member.communicationDisabledUntilTimestamp;
  return Number.isFinite(ts) ? new Date(ts) : null;
};
const tsOf = (member) => {
  const ts = member.communicationDisabledUntilTimestamp;
  if (Number.isFinite(ts)) return ts;
  const d = member.communicationDisabledUntil;
  return d ? d.getTime() : null;
};

/**
 * Diffs a member update. With `oldKnown === false` (the old member was a partial clone: empty roles, null
 * nickname) nothing but an active timeout can be reported, and that as `active-unknown` (R033/R111).
 * Timeouts use isCommunicationDisabled() on both sides, because Discord keeps a past timestamp after expiry
 * (R112/R261).
 * @param {{ oldMember: object, newMember: object, oldKnown: boolean, now?: number }} p
 * @returns {{ nickname: object|null, roles: object|null, avatar: object|null, timeout: object|null }}
 */
export function diffMemberUpdate({ oldMember, newMember, oldKnown }) {
  const result = { nickname: null, roles: null, avatar: null, timeout: null };
  if (!oldKnown) {
    if (newMember.isCommunicationDisabled?.()) result.timeout = { kind: 'active-unknown', until: untilOf(newMember) };
    return result;
  }

  if ((oldMember.nickname ?? null) !== (newMember.nickname ?? null)) {
    result.nickname = { before: oldMember.nickname ?? null, after: newMember.nickname ?? null };
  }

  const guildId = newMember.guild?.id;
  const oldRoles = keys(oldMember.roles?.cache);
  const newRoles = keys(newMember.roles?.cache);
  const added = newRoles.filter((id) => !oldRoles.includes(id) && id !== guildId);
  const removed = oldRoles.filter((id) => !newRoles.includes(id) && id !== guildId);
  if (added.length || removed.length) result.roles = { added, removed };

  if ((oldMember.avatar ?? null) !== (newMember.avatar ?? null)) {
    result.avatar = { before: oldMember.avatarURL?.({ size: 256 }) ?? null, after: newMember.avatarURL?.({ size: 256 }) ?? null };
  }

  const was = Boolean(oldMember.isCommunicationDisabled?.());
  const is = Boolean(newMember.isCommunicationDisabled?.());
  if (!was && is) result.timeout = { kind: 'added', until: untilOf(newMember) };
  else if (was && !is) result.timeout = { kind: 'removed' };
  else if (was && is && tsOf(oldMember) !== tsOf(newMember)) {
    result.timeout = { kind: 'changed', from: untilOf(oldMember), until: untilOf(newMember) };
  }
  return result;
}

const memberEmbed = (color, title, user) => {
  const embed = logEmbed(color, title);
  setAuthor(embed, user);
  return embed;
};

/** @param {{ member: object, roles: { added: string[], removed: string[] } }} p */
export function buildMemberRolesEmbed({ member, roles }) {
  const embed = memberEmbed(LOG_COLORS.UPDATE, '🎭 Member Roles Changed', member.user);
  const fields = [field('Member', userLine(member.user))];
  if (roles.added.length) fields.push(field('✅ Added', joinWithLimit(roles.added.map(roleRef), { sep: ', ', max: 1024 })));
  if (roles.removed.length) fields.push(field('❌ Removed', joinWithLimit(roles.removed.map(roleRef), { sep: ', ', max: 1024 })));
  setFooter(embed, `User ID: ${member.user?.id ?? member.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/** @param {{ member: object, nickname: { before: string|null, after: string|null } }} p */
export function buildNicknameEmbed({ member, nickname }) {
  const embed = memberEmbed(LOG_COLORS.UPDATE, '📝 Nickname Changed', member.user);
  const none = `*${clampText(member.user?.username ?? 'user', 100).replace(/[*_~`|]/g, '')}* (no nickname)`;
  setFooter(embed, `User ID: ${member.user?.id ?? member.id}`);
  setFields(embed, [
    field('Member', userLine(member.user)),
    field('Before', nickname.before ? clampText(nickname.before, 1024) : none, true),
    field('After', nickname.after ? clampText(nickname.after, 1024) : none, true),
  ]);
  return fitEmbed(embed);
}

const until = (date) => (date ? `${discordTimestamp(date, 'f')} (${discordTimestamp(date, 'R')})` : 'Unknown');

/** @param {{ member: object, timeout: { kind: string, until?: Date|null, from?: Date|null } }} p */
export function buildTimeoutEmbed({ member, timeout }) {
  const user = member.user;
  let embed;
  const fields = [field('Member', userLine(user))];
  let footer = `User ID: ${user?.id ?? member.id}`;
  switch (timeout.kind) {
    case 'removed':
      embed = memberEmbed(LOG_COLORS.CREATE, '🔊 Timeout Removed', user);
      setDescription(embed, 'The timeout was lifted before it expired.');
      break;
    case 'changed':
      embed = memberEmbed(LOG_COLORS.ALERT, '⏱️ Timeout Changed', user);
      fields.push(field('Before', until(timeout.from), true), field('After', until(timeout.until), true));
      break;
    case 'active-unknown':
      embed = memberEmbed(LOG_COLORS.ALERT, '⏱️ Member Timed Out', user);
      fields.push(field('Until', until(timeout.until)));
      footer += ' · previous state not cached';
      break;
    default: // 'added'
      embed = memberEmbed(LOG_COLORS.ALERT, '⏱️ Member Timed Out', user);
      fields.push(field('Until', until(timeout.until)));
  }
  setFooter(embed, footer);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/** @param {{ member: object, avatar: { before: string|null, after: string|null } }} p */
export function buildAvatarEmbed({ member, avatar }) {
  const embed = memberEmbed(LOG_COLORS.UPDATE, '🖼️ Server Avatar Changed', member.user);
  setThumb(embed, avatar.after);
  setFooter(embed, `User ID: ${member.user?.id ?? member.id}`);
  setFields(embed, [
    field('Member', userLine(member.user)),
    field('Before', avatar.before ?? '(none)'),
    field('After', avatar.after ?? '(none)'),
  ]);
  return fitEmbed(embed);
}

// ── User updates (global name / avatar) ───────────────────────────────────────

/**
 * Diffs a global user update (the `userUpdate` hook point): username, display name (globalName) and avatar.
 * @returns {{ username: object|null, displayName: object|null, avatar: object|null }}
 */
export function diffUser(oldUser, newUser) {
  const url = (u) => {
    try { return u?.displayAvatarURL?.({ size: 256 }) ?? null; } catch { return null; }
  };
  const out = { username: null, displayName: null, avatar: null };
  if ((oldUser?.username ?? null) !== (newUser?.username ?? null)) out.username = { before: oldUser?.username ?? null, after: newUser?.username ?? null };
  if ((oldUser?.globalName ?? null) !== (newUser?.globalName ?? null)) out.displayName = { before: oldUser?.globalName ?? null, after: newUser?.globalName ?? null };
  const before = url(oldUser);
  const after = url(newUser);
  if (before !== after) out.avatar = { before, after };
  return out;
}

/** @param {{ user: object, diff: ReturnType<typeof diffUser> }} p  null when nothing changed */
export function buildUserUpdateEmbed({ user, diff }) {
  if (!diff || (!diff.username && !diff.displayName && !diff.avatar)) return null;
  const embed = memberEmbed(LOG_COLORS.UPDATE, '👤 User Updated', user);
  const fields = [field('User', userLine(user))];
  if (diff.username) fields.push(field('Username', change(diff.username.before, diff.username.after)));
  if (diff.displayName) fields.push(field('Display name', change(diff.displayName.before, diff.displayName.after)));
  if (diff.avatar) {
    fields.push(field('Avatar', 'Avatar changed'));
    setThumb(embed, diff.avatar.after);
  }
  setFooter(embed, `User ID: ${user?.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Bans (§6.4.4) ─────────────────────────────────────────────────────────────

/** @param {{ user: object, executorId?: string|null, reason?: string|null, unban?: boolean, botId?: string|null }} p */
export function buildBanEmbed({ user, executorId = null, reason = null, unban = false, botId = null }) {
  const embed = logEmbed(unban ? LOG_COLORS.CREATE : LOG_COLORS.DELETE, unban ? '✅ Member Unbanned' : '🔨 Member Banned');
  setThumb(embed, avatarOf(user));
  let by = 'Unknown';
  if (executorId) by = `<@${executorId}>${botId && executorId === botId ? ' (via bot command)' : ''}`;
  setFooter(embed, `User ID: ${user?.id}`);
  setFields(embed, [
    field('User', userLine(user), true),
    field('By', by, true),
    field('Reason', reason ? clampText(reason, 1024) : 'No reason given'),
  ]);
  return fitEmbed(embed);
}

// ── Voice (§6.5) ──────────────────────────────────────────────────────────────

/**
 * Classifies a voice state change by channel ids (R269): a deleted channel resolves to null on the state but
 * its id survives, so a leave is still a leave.
 * @param {{ channelId?: string|null }} oldState
 * @param {{ channelId?: string|null }} newState
 * @returns {{ kind: 'join'|'leave'|'move'|'state'|null, oldId: string|null, newId: string|null, changes: string[] }}
 */
export function classifyVoice(oldState, newState) {
  const oldId = oldState?.channelId ?? null;
  const newId = newState?.channelId ?? null;
  const out = { kind: null, oldId, newId, changes: [] };
  if (!oldId && !newId) return out;
  if (!oldId) return { ...out, kind: 'join' };
  if (!newId) return { ...out, kind: 'leave' };
  if (oldId !== newId) return { ...out, kind: 'move' };

  const changes = [];
  if (!oldState.serverMute && newState.serverMute) changes.push('Server muted');
  if (oldState.serverMute && !newState.serverMute) changes.push('Server unmuted');
  if (!oldState.serverDeaf && newState.serverDeaf) changes.push('Server deafened');
  if (oldState.serverDeaf && !newState.serverDeaf) changes.push('Server undeafened');
  if (!oldState.selfDeaf && newState.selfDeaf) changes.push('Self-deafened');
  if (oldState.selfDeaf && !newState.selfDeaf) changes.push('Self-undeafened');
  if (!oldState.selfMute && newState.selfMute) changes.push('Self-muted');
  if (oldState.selfMute && !newState.selfMute) changes.push('Self-unmuted');
  if (!oldState.streaming && newState.streaming) changes.push('Started streaming');
  if (oldState.streaming && !newState.streaming) changes.push('Stopped streaming');
  if (!changes.length) return out;
  return { ...out, kind: 'state', changes };
}

/** @param {{ member: object, guild: object, c: ReturnType<typeof classifyVoice> }} p */
export function buildVoiceEmbed({ member, guild, c }) {
  const id = member.id ?? member.user?.id;
  const meta = {
    join:  ['🔊 Joined Voice', LOG_COLORS.CREATE, `<@${id}> joined ${channelRef(guild, c.newId)}`],
    leave: ['🔇 Left Voice', LOG_COLORS.DELETE, `<@${id}> left ${channelRef(guild, c.oldId)}`],
    move:  ['↔️ Moved Voice Channel', LOG_COLORS.UPDATE, `<@${id}> moved from ${channelRef(guild, c.oldId)} → ${channelRef(guild, c.newId)}`],
    state: ['🔔 Voice State Changed', LOG_COLORS.NEUTRAL, `<@${id}> in ${channelRef(guild, c.newId ?? c.oldId)}\n${(c.changes ?? []).join(', ')}`],
  }[c.kind];
  if (!meta) return null;
  const embed = logEmbed(meta[1], meta[0]);
  setDescription(embed, meta[2]);
  setAuthor(embed, member.user);
  setFooter(embed, `User ID: ${id}`);
  return fitEmbed(embed);
}

// ── Channels (§6.6) ───────────────────────────────────────────────────────────

/** @param {{ channel: object }} p */
export function buildChannelCreateEmbed({ channel }) {
  const embed = logEmbed(LOG_COLORS.CREATE, '📢 Channel Created');
  const fields = [
    field('Name', `<#${channel.id}> \`${clampText(channel.name, 100).replace(/`/g, "'")}\``, true),
    field('Type', channelTypeLabel(channel.type), true),
  ];
  if (channel.parent?.name) fields.push(field('Category', channel.parent.name, true));
  if (channel.topic) fields.push(field('Topic', clampText(channel.topic, 1024)));
  setFooter(embed, `Channel ID: ${channel.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

const target = (o) => (o.type === 1 || o.type === 'member' ? `<@${o.id}>` : `<@&${o.id}>`);

/**
 * One line per permission overwrite that was added, removed or changed.
 * @returns {string[]}
 */
function diffOverwrites(oldChannel, newChannel) {
  const oldMap = new Map(values(oldChannel.permissionOverwrites?.cache).map((o) => [o.id, o]));
  const newMap = new Map(values(newChannel.permissionOverwrites?.cache).map((o) => [o.id, o]));
  const names = (b) => permissionNames(b).join(', ');
  const lines = [];

  for (const [id, n] of newMap) {
    const o = oldMap.get(id);
    const nAllow = bits(n.allow);
    const nDeny = bits(n.deny);
    if (!o) {
      const parts = [];
      if (nAllow) parts.push(`✅ ${names(nAllow)}`);
      if (nDeny) parts.push(`❌ ${names(nDeny)}`);
      lines.push(`${target(n)}: added${parts.length ? ` · ${parts.join(' · ')}` : ''}`);
      continue;
    }
    const oAllow = bits(o.allow);
    const oDeny = bits(o.deny);
    if (oAllow === nAllow && oDeny === nDeny) continue;
    const allowed = nAllow & ~oAllow;
    const denied = nDeny & ~oDeny;
    const reset = (oAllow | oDeny) & ~(nAllow | nDeny);
    const parts = [];
    if (allowed) parts.push(`✅ ${names(allowed)}`);
    if (denied) parts.push(`❌ ${names(denied)}`);
    if (reset) parts.push(`↩️ ${names(reset)}`);
    lines.push(`${target(n)}: ${parts.join(' · ')}`);
  }
  for (const [id, o] of oldMap) {
    if (!newMap.has(id)) lines.push(`${target(o)}: removed`);
  }
  return lines.map((l) => clampText(l, 500));
}

/**
 * Diffs a channel update into { name, value } lines. A position-only change gives [] (reordering a category
 * fires many events and is never logged).
 * @param {object} oldChannel
 * @param {object} newChannel
 * @returns {Array<{ name: string, value: string }>}
 */
export function diffChannel(oldChannel, newChannel) {
  const lines = [];
  const add = (name, value) => lines.push({ name, value: clampText(value, 1024) });
  const o = oldChannel;
  const n = newChannel;

  if (o.name !== n.name) add('Name', change(o.name, n.name));
  if ((o.topic ?? '') !== (n.topic ?? '')) {
    const half = (t) => (t ? clampText(t, 500) : '(none)');
    add('Topic', `**Before:** ${half(o.topic)}\n**After:** ${half(n.topic)}`);
  }
  if (Boolean(o.nsfw) !== Boolean(n.nsfw)) add('NSFW', yesNo(n.nsfw));
  if ((o.rateLimitPerUser ?? 0) !== (n.rateLimitPerUser ?? 0)) {
    const fmt = (s) => (s ? formatDurationLong(s * 1000) : 'off');
    add('Slowmode', `${fmt(o.rateLimitPerUser)} → ${fmt(n.rateLimitPerUser)}`);
  }
  if ((o.parentId ?? null) !== (n.parentId ?? null)) {
    const fmt = (c) => (c.parent?.name ?? (c.parentId ? `<#${c.parentId}>` : '(none)'));
    add('Category', `${fmt(o)} → ${fmt(n)}`);
  }
  if (o.type !== n.type) add('Type', `${channelTypeLabel(o.type)} → ${channelTypeLabel(n.type)}`);
  if (o.bitrate !== undefined && n.bitrate !== undefined && o.bitrate !== n.bitrate) {
    add('Bitrate', `${Math.round(o.bitrate / 1000)} kbps → ${Math.round(n.bitrate / 1000)} kbps`);
  }
  if (o.userLimit !== undefined && n.userLimit !== undefined && o.userLimit !== n.userLimit) {
    const fmt = (v) => (v ? String(v) : 'unlimited');
    add('User Limit', `${fmt(o.userLimit)} → ${fmt(n.userLimit)}`);
  }
  if (o.rtcRegion !== undefined && n.rtcRegion !== undefined && (o.rtcRegion ?? null) !== (n.rtcRegion ?? null)) {
    add('Region', `${o.rtcRegion ?? 'automatic'} → ${n.rtcRegion ?? 'automatic'}`);
  }
  const overwrites = diffOverwrites(o, n);
  if (overwrites.length) add('Permissions', joinWithLimit(overwrites, { sep: '\n', max: 1024 }));
  return lines;
}

/** @param {{ channel: object, lines: Array<{ name: string, value: string }> }} p  null when there are no lines */
export function buildChannelUpdateEmbed({ channel, lines }) {
  if (!lines?.length) return null;
  const embed = logEmbed(LOG_COLORS.UPDATE, '🛠️ Channel Updated');
  setDescription(embed, `<#${channel.id}> \`${clampText(channel.name, 100).replace(/`/g, "'")}\``);
  setFooter(embed, `Channel ID: ${channel.id}`);
  setFields(embed, lines.map((l) => field(l.name, l.value)));
  return fitEmbed(embed);
}

/** @param {{ channel: object, occupants?: string[], botReason?: string|null }} p */
export function buildChannelDeleteEmbed({ channel, occupants = [], botReason = null }) {
  const embed = logEmbed(LOG_COLORS.DELETE, '🗑️ Channel Deleted');
  const fields = [
    field('Name', `\`${clampText(channel.name, 100).replace(/`/g, "'")}\``, true),
    field('Type', channelTypeLabel(channel.type), true),
  ];
  if (channel.parent?.name) fields.push(field('Category', channel.parent.name, true));
  if (occupants.length) {
    fields.push(field(`Connected Members (${occupants.length})`, joinWithLimit(occupants.map((id) => `<@${id}>`), { sep: ', ', max: 1024 })));
  }
  if (botReason) fields.push(field('Deleted By', `the bot (${clampText(botReason, 100)})`));
  setFooter(embed, `Channel ID: ${channel.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Threads ───────────────────────────────────────────────────────────────────

const ARCHIVE_LABELS = Object.freeze({ 60: '1 hour', 1440: '24 hours', 4320: '3 days', 10080: '1 week' });
const archiveLabel = (minutes) => (Number.isFinite(Number(minutes)) && Number(minutes) > 0
  ? (ARCHIVE_LABELS[Number(minutes)] ?? formatDurationLong(Number(minutes) * 60_000))
  : 'Unknown');
const threadName = (thread) => clampText(thread?.name ?? 'unknown', 100).replace(/`/g, "'");

/** Name of a forum tag: the parent's tag list when it is cached, otherwise the id. */
function tagLabel(thread, id) {
  const tags = thread?.parent?.availableTags;
  const tag = Array.isArray(tags) ? tags.find((t) => t.id === id) : null;
  return tag?.name ? clampText(tag.name, 50) : String(id);
}

/** @param {{ thread: object }} p */
export function buildThreadCreateEmbed({ thread }) {
  const embed = logEmbed(LOG_COLORS.CREATE, '🧵 Thread Created');
  const fields = [
    field('Thread', `<#${thread.id}> \`${threadName(thread)}\``, true),
    field('Type', channelTypeLabel(thread.type), true),
  ];
  if (thread.parentId) fields.push(field('Channel', `<#${thread.parentId}>`, true));
  if (thread.ownerId) fields.push(field('Started By', `<@${thread.ownerId}>`, true));
  if (thread.autoArchiveDuration) fields.push(field('Auto-archive', archiveLabel(thread.autoArchiveDuration), true));
  if (thread.rateLimitPerUser) fields.push(field('Slowmode', formatDurationLong(thread.rateLimitPerUser * 1000), true));
  setFooter(embed, `Thread ID: ${thread.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/**
 * Diffs a thread update into { name, value } lines: name, locked, slowmode, auto-archive duration, invitable
 * (private threads) and forum tags. Archiving and unarchiving are deliberately NOT lines: threads archive
 * themselves after inactivity and wake up on the next message, which would flood the log. A change that
 * touches none of the logged properties gives [].
 * @param {object} oldThread
 * @param {object} newThread
 * @returns {Array<{ name: string, value: string }>}
 */
export function diffThread(oldThread, newThread) {
  const lines = [];
  const add = (name, value) => lines.push({ name, value: clampText(value, 1024) });
  const o = oldThread ?? {};
  const n = newThread ?? {};

  if (o.name !== undefined && n.name !== undefined && o.name !== n.name) add('Name', change(o.name, n.name));
  if (o.locked !== undefined && n.locked !== undefined && Boolean(o.locked) !== Boolean(n.locked)) {
    add('Locked', n.locked ? 'Locked' : 'Unlocked');
  }
  if (o.rateLimitPerUser !== undefined && n.rateLimitPerUser !== undefined && (o.rateLimitPerUser ?? 0) !== (n.rateLimitPerUser ?? 0)) {
    const fmt = (s) => (s ? formatDurationLong(s * 1000) : 'off');
    add('Slowmode', `${fmt(o.rateLimitPerUser)} → ${fmt(n.rateLimitPerUser)}`);
  }
  if (o.autoArchiveDuration != null && n.autoArchiveDuration != null && o.autoArchiveDuration !== n.autoArchiveDuration) {
    add('Auto-archive', `${archiveLabel(o.autoArchiveDuration)} → ${archiveLabel(n.autoArchiveDuration)}`);
  }
  if (o.invitable != null && n.invitable != null && Boolean(o.invitable) !== Boolean(n.invitable)) {
    add('Invitable', yesNo(n.invitable));
  }
  if (Array.isArray(o.appliedTags) && Array.isArray(n.appliedTags) && !sameSet(o.appliedTags, n.appliedTags)) {
    const added = n.appliedTags.filter((id) => !o.appliedTags.includes(id)).map((id) => `+ ${tagLabel(n, id)}`);
    const removed = o.appliedTags.filter((id) => !n.appliedTags.includes(id)).map((id) => `- ${tagLabel(n, id)}`);
    add('Tags', joinWithLimit([...added, ...removed], { sep: '\n', max: 1024 }));
  }
  return lines;
}

/** @param {{ thread: object, lines: Array<{ name: string, value: string }> }} p  null when there are no lines */
export function buildThreadUpdateEmbed({ thread, lines }) {
  if (!lines?.length) return null;
  const embed = logEmbed(LOG_COLORS.UPDATE, '🧵 Thread Updated');
  setDescription(embed, `<#${thread.id}> \`${threadName(thread)}\``);
  setFooter(embed, `Thread ID: ${thread.id}`);
  setFields(embed, lines.map((l) => field(l.name, l.value)));
  return fitEmbed(embed);
}

/** @param {{ thread: object, botReason?: string|null }} p */
export function buildThreadDeleteEmbed({ thread, botReason = null }) {
  const embed = logEmbed(LOG_COLORS.DELETE, '🗑️ Thread Deleted');
  const fields = [
    field('Name', `\`${threadName(thread)}\``, true),
    field('Type', channelTypeLabel(thread.type), true),
  ];
  if (thread.parentId) fields.push(field('Channel', channelRef(thread.guild, thread.parentId), true));
  if (thread.ownerId) fields.push(field('Started By', `<@${thread.ownerId}>`, true));
  if (botReason) fields.push(field('Deleted By', `the bot (${clampText(botReason, 100)})`));
  setFooter(embed, `Thread ID: ${thread.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Roles (§6.7) ──────────────────────────────────────────────────────────────

const permList = (permissions) => permissionNames(typeof permissions?.toArray === 'function' ? permissions.toArray() : permissions);

/** @param {{ role: object }} p */
export function buildRoleCreateEmbed({ role }) {
  const embed = logEmbed(LOG_COLORS.CREATE, '➕ Role Created');
  const perms = permList(role.permissions);
  const fields = [
    field('Role', `<@&${role.id}> \`${clampText(role.name, 100).replace(/`/g, "'")}\``),
    field('Color', role.hexColor ?? '#000000', true),
    field('Hoisted', yesNo(role.hoist), true),
    field('Mentionable', yesNo(role.mentionable), true),
  ];
  if (role.managed) fields.push(field('Managed', 'Yes (integration or bot role)'));
  fields.push(field('Permissions', perms.length ? joinWithLimit(perms, { sep: ', ', max: 1024 }) : 'None'));
  setFooter(embed, `Role ID: ${role.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/**
 * Diffs a role update. A position-only change gives [].
 * @returns {Array<{ name: string, value: string }>}
 */
export function diffRole(oldRole, newRole) {
  const lines = [];
  const add = (name, value) => lines.push({ name, value: clampText(value, 1024) });
  if (oldRole.name !== newRole.name) add('Name', change(oldRole.name, newRole.name));
  if (oldRole.hexColor !== newRole.hexColor) add('Color', change(oldRole.hexColor, newRole.hexColor));
  if (Boolean(oldRole.hoist) !== Boolean(newRole.hoist)) add('Hoisted', `${yesNo(oldRole.hoist)} → ${yesNo(newRole.hoist)}`);
  if (Boolean(oldRole.mentionable) !== Boolean(newRole.mentionable)) add('Mentionable', `${yesNo(oldRole.mentionable)} → ${yesNo(newRole.mentionable)}`);
  if ((oldRole.icon ?? null) !== (newRole.icon ?? null) || (oldRole.unicodeEmoji ?? null) !== (newRole.unicodeEmoji ?? null)) add('Icon', 'changed');
  const oldBits = bits(oldRole.permissions);
  const newBits = bits(newRole.permissions);
  const added = newBits & ~oldBits;
  const removed = oldBits & ~newBits;
  if (added) add('Permissions Added', joinWithLimit(permissionNames(added), { sep: ', ', max: 1024 }));
  if (removed) add('Permissions Removed', joinWithLimit(permissionNames(removed), { sep: ', ', max: 1024 }));
  return lines;
}

/** @param {{ role: object, lines: Array<{ name: string, value: string }> }} p  null when there are no lines */
export function buildRoleUpdateEmbed({ role, lines }) {
  if (!lines?.length) return null;
  const embed = logEmbed(LOG_COLORS.UPDATE, '🛠️ Role Updated');
  setDescription(embed, `<@&${role.id}> \`${clampText(role.name, 100).replace(/`/g, "'")}\``);
  setFooter(embed, `Role ID: ${role.id}`);
  setFields(embed, lines.map((l) => field(l.name, l.value)));
  return fitEmbed(embed);
}

/** @param {{ role: object }} p */
export function buildRoleDeleteEmbed({ role }) {
  const embed = logEmbed(LOG_COLORS.DELETE, '➖ Role Deleted');
  const fields = [
    field('Name', `\`${clampText(role.name, 100).replace(/`/g, "'")}\``, true),
    field('Color', role.hexColor ?? '#000000', true),
  ];
  if (role.createdAt) fields.push(field('Created', discordTimestamp(role.createdAt, 'R'), true));
  setFooter(embed, `Role ID: ${role.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

// ── Server (§6.8) ─────────────────────────────────────────────────────────────

/**
 * Diffs a guild update. Properties that are not listed (member counts, boost counts, …) produce nothing.
 * @returns {Array<{ name: string, value: string }>}
 */
export function diffGuild(oldGuild, newGuild) {
  const lines = [];
  const add = (name, value) => lines.push({ name, value: clampText(value, 1024) });
  const o = oldGuild;
  const n = newGuild;
  const differs = (key) => (o[key] ?? null) !== (n[key] ?? null);
  const enumName = (e, v) => (v === null || v === undefined ? '(none)' : (e[v] ?? String(v)));
  const ch = (id) => (id ? `<#${id}>` : '*(none)*');

  if (differs('name')) add('Name', change(o.name, n.name));
  if (differs('icon')) add('Icon', 'changed');
  if (differs('banner')) add('Banner', 'changed');
  if (differs('splash')) add('Invite Splash', 'changed');
  if (differs('description')) {
    const half = (t) => (t ? clampText(t, 500) : '(none)');
    add('Description', `**Before:** ${half(o.description)}\n**After:** ${half(n.description)}`);
  }
  if (differs('ownerId')) add('Owner', `<@${o.ownerId}> → <@${n.ownerId}>`);
  if (differs('verificationLevel')) add('Verification Level', `${enumName(GuildVerificationLevel, o.verificationLevel)} → ${enumName(GuildVerificationLevel, n.verificationLevel)}`);
  if (differs('explicitContentFilter')) add('Explicit Content Filter', `${enumName(GuildExplicitContentFilter, o.explicitContentFilter)} → ${enumName(GuildExplicitContentFilter, n.explicitContentFilter)}`);
  if (differs('defaultMessageNotifications')) add('Default Notifications', `${enumName(GuildDefaultMessageNotifications, o.defaultMessageNotifications)} → ${enumName(GuildDefaultMessageNotifications, n.defaultMessageNotifications)}`);
  if (differs('afkChannelId')) add('AFK Channel', `${ch(o.afkChannelId)} → ${ch(n.afkChannelId)}`);
  if (differs('afkTimeout')) add('AFK Timeout', `${formatDurationLong((o.afkTimeout ?? 0) * 1000)} → ${formatDurationLong((n.afkTimeout ?? 0) * 1000)}`);
  if (differs('systemChannelId')) add('System Channel', `${ch(o.systemChannelId)} → ${ch(n.systemChannelId)}`);
  if (differs('rulesChannelId')) add('Rules Channel', `${ch(o.rulesChannelId)} → ${ch(n.rulesChannelId)}`);
  if (differs('publicUpdatesChannelId')) add('Community Updates Channel', `${ch(o.publicUpdatesChannelId)} → ${ch(n.publicUpdatesChannelId)}`);
  if (differs('vanityURLCode')) add('Vanity URL', change(o.vanityURLCode, n.vanityURLCode));
  if (differs('preferredLocale')) add('Preferred Locale', change(o.preferredLocale, n.preferredLocale));
  if (differs('premiumTier')) add('Boost Level', `${o.premiumTier ?? 0} → ${n.premiumTier ?? 0}`);
  if (differs('mfaLevel')) add('2FA Requirement', `${enumName(GuildMFALevel, o.mfaLevel)} → ${enumName(GuildMFALevel, n.mfaLevel)}`);
  if (differs('nsfwLevel')) add('NSFW Level', `${enumName(GuildNSFWLevel, o.nsfwLevel)} → ${enumName(GuildNSFWLevel, n.nsfwLevel)}`);
  if (differs('premiumProgressBarEnabled')) add('Boost Progress Bar', `${yesNo(o.premiumProgressBarEnabled)} → ${yesNo(n.premiumProgressBarEnabled)}`);
  return lines;
}

/** @param {{ oldGuild: object, newGuild: object, lines: Array<{ name: string, value: string }> }} p  null when there are no lines */
export function buildGuildUpdateEmbed({ oldGuild, newGuild, lines }) {
  if (!lines?.length) return null;
  const embed = logEmbed(LOG_COLORS.UPDATE, '🏠 Server Edited');
  if (oldGuild.icon !== newGuild.icon) {
    try { setThumb(embed, newGuild.iconURL?.() ?? null); } catch { /* no icon to show */ }
  }
  setFooter(embed, `Server ID: ${newGuild.id}`);
  setFields(embed, lines.map((l) => field(l.name, l.value)));
  return fitEmbed(embed);
}

// ── Emojis (§6.9) ─────────────────────────────────────────────────────────────

const emojiImage = (emoji) => {
  try { return emoji.imageURL?.() ?? null; } catch { return null; }
};

/** @param {{ emoji: object }} p */
export function buildEmojiCreateEmbed({ emoji }) {
  const embed = logEmbed(LOG_COLORS.CREATE, '😀 Emoji Created');
  setThumb(embed, emojiImage(emoji));
  const fields = [
    field('Emoji', `${emoji.toString()} \`:${clampText(emoji.name, 64)}:\``),
    field('Animated', yesNo(emoji.animated), true),
  ];
  const roles = keys(emoji.roles?.cache);
  if (roles.length) fields.push(field('Restricted To', joinWithLimit(roles.map(roleRef), { sep: ', ', max: 1024 })));
  setFooter(embed, `Emoji ID: ${emoji.id}`);
  setFields(embed, fields);
  return fitEmbed(embed);
}

/** @returns {Array<{ name: string, value: string }>} */
export function diffEmoji(oldEmoji, newEmoji) {
  const lines = [];
  if (oldEmoji.name !== newEmoji.name) lines.push({ name: 'Name', value: change(oldEmoji.name, newEmoji.name) });
  const before = keys(oldEmoji.roles?.cache);
  const after = keys(newEmoji.roles?.cache);
  if (!sameSet(before, after)) {
    const added = after.filter((id) => !before.includes(id)).map(roleRef);
    const removed = before.filter((id) => !after.includes(id)).map(roleRef);
    const parts = [];
    if (added.length) parts.push(`✅ ${joinWithLimit(added, { sep: ', ', max: 480 })}`);
    if (removed.length) parts.push(`❌ ${joinWithLimit(removed, { sep: ', ', max: 480 })}`);
    lines.push({ name: 'Roles', value: parts.join('\n') });
  }
  return lines;
}

/** @param {{ emoji: object, lines: Array<{ name: string, value: string }> }} p  null when there are no lines */
export function buildEmojiUpdateEmbed({ emoji, lines }) {
  if (!lines?.length) return null;
  const embed = logEmbed(LOG_COLORS.UPDATE, '😀 Emoji Updated');
  setThumb(embed, emojiImage(emoji));
  setDescription(embed, `${emoji.toString()} \`:${clampText(emoji.name, 64)}:\``);
  setFooter(embed, `Emoji ID: ${emoji.id}`);
  setFields(embed, lines.map((l) => field(l.name, l.value)));
  return fitEmbed(embed);
}

/** @param {{ emoji: object }} p */
export function buildEmojiDeleteEmbed({ emoji }) {
  const embed = logEmbed(LOG_COLORS.DELETE, '😶 Emoji Deleted');
  setThumb(embed, emojiImage(emoji));
  setFooter(embed, `Emoji ID: ${emoji.id}`);
  setFields(embed, [field('Name', `\`:${clampText(emoji.name, 64)}:\``)]);
  return fitEmbed(embed);
}

// ── Invites (§6.10) ───────────────────────────────────────────────────────────

/** @param {{ invite: object }} p */
export function buildInviteCreateEmbed({ invite }) {
  const embed = logEmbed(LOG_COLORS.UPDATE, '🔗 Invite Created');
  setFooter(embed, `Code: ${invite.code}`);
  setFields(embed, [
    field('Code', `discord.gg/${invite.code}`, true),
    field('Channel', invite.channel?.id ? `<#${invite.channel.id}>` : 'Unknown', true),
    field('Inviter', invite.inviter ? userLine(invite.inviter) : 'Unknown', true),
    field('Max Uses', invite.maxUses ? String(invite.maxUses) : '∞', true),
    field('Expires', invite.expiresAt ? discordTimestamp(invite.expiresAt, 'R') : 'Never', true),
    field('Temporary', yesNo(invite.temporary), true),
  ]);
  return fitEmbed(embed);
}

/** @param {{ invite: object }} p */
export function buildInviteDeleteEmbed({ invite }) {
  const embed = logEmbed(LOG_COLORS.DELETE, '🔗 Invite Deleted / Expired');
  setFooter(embed, `Code: ${invite.code}`);
  setFields(embed, [
    field('Code', `discord.gg/${invite.code}`, true),
    field('Channel', invite.channel?.id ? `<#${invite.channel.id}>` : 'Unknown', true),
  ]);
  return fitEmbed(embed);
}
