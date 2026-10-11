import { SlashCommandBuilder, PermissionFlagsBits, ChannelType } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import { parseDuration, formatDurationLong } from '../../utils/parseDuration.js';
import {
  replyError, requireBotPermissions, safeDefer, safeReply, respondAutocomplete, isInteractionGone,
} from '../../utils/interactionHelpers.js';
import { clampText } from '../../utils/text.js';
import { withTimeout, TimeoutError, ackBudgetMs } from '../../utils/async.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { resolvePermissionLevel } from '../../utils/permissionChecker.js';
import logger from '../../utils/logger.js';

export const module = 'moderation';
export const permissionLevel = PERMISSION_LEVELS.MODERATOR;
// gated: default true; allowInDMs: default false (guild-only)

export const SLOWMODE_MAX_SECONDS = 21_600; // Discord's rate_limit_per_user limit, 6 h
const OFF_WORDS = new Set(['off', 'none', 'disable', 'disabled', 'false', 'no', '0', '0s']);
const SLOWMODE_TYPES = new Set([
  ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildStageVoice,
  ChannelType.GuildForum, ChannelType.GuildMedia,
  ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread,
]);
const PRESETS = [
  ['Off', 'off'], ['5 seconds', '5s'], ['10 seconds', '10s'], ['15 seconds', '15s'], ['30 seconds', '30s'],
  ['1 minute', '1m'], ['2 minutes', '2m'], ['5 minutes', '5m'], ['10 minutes', '10m'], ['15 minutes', '15m'],
  ['30 minutes', '30m'], ['1 hour', '1h'], ['2 hours', '2h'], ['6 hours', '6h'],
];

// ── Command definition ─────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('slowmode')
  .setDescription("Set or remove a channel's slowmode (MEE6 /slowmode)")
  .setDefaultMemberPermissions(PermissionFlagsBits.ModerateMembers)
  .addStringOption((o) => o.setName('duration').setDescription('Interval such as 10s, 5m, 1h (max 6h), or off').setRequired(true).setMaxLength(16).setAutocomplete(true))
  .addChannelOption((o) => o.setName('channel').setDescription('Channel or thread (default: this one)').addChannelTypes(
    ChannelType.GuildText, ChannelType.GuildVoice, ChannelType.GuildStageVoice, ChannelType.GuildForum, ChannelType.GuildMedia,
    ChannelType.PublicThread, ChannelType.PrivateThread, ChannelType.AnnouncementThread))
  .addStringOption((o) => o.setName('reason').setDescription('Reason (shown in the audit log)').setMaxLength(512));

// ── Pure helpers ───────────────────────────────────────────────────────────────

/**
 * Parses the `duration` option. A bare number means seconds; 'off' (and friends) and 0 mean no slowmode.
 * @param {unknown} input
 * @returns {{ ok: true, seconds: number } | { ok: false, code: 'invalid'|'too-short'|'too-long' }}
 */
export function parseSlowmode(input) {
  const s = String(input ?? '').trim().toLowerCase();
  if (!s || s.length > 16) return { ok: false, code: 'invalid' };
  if (OFF_WORDS.has(s)) return { ok: true, seconds: 0 };

  let seconds;
  if (/^\d+$/.test(s)) {
    seconds = Number(s);
    if (seconds === 0) return { ok: true, seconds: 0 };
  } else {
    const ms = parseDuration(s);
    if (ms === null) return { ok: false, code: 'invalid' };
    seconds = Math.round(ms / 1000);
  }
  if (seconds < 1) return { ok: false, code: 'too-short' };
  if (seconds > SLOWMODE_MAX_SECONDS) return { ok: false, code: 'too-long' };
  return { ok: true, seconds };
}

/** @param {number} seconds @returns {string} 'Off' for 0, else words such as '2 hours 30 minutes' */
export function formatSlowmode(seconds) {
  return seconds === 0 ? 'Off' : formatDurationLong(seconds * 1000);
}

/**
 * @param {{ channelId: string, oldSeconds: number, newSeconds: number, reason?: string|null,
 *   moderatorId?: string|null, forum?: boolean }} opts
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildSlowmodeEmbed({ channelId, oldSeconds, newSeconds, reason = null, moderatorId = null, forum = false }) {
  const title = newSeconds === 0 ? 'Slowmode Disabled' : oldSeconds === 0 ? 'Slowmode Enabled' : 'Slowmode Updated';
  const lines = [
    `**Channel:** <#${channelId}>`,
    `**Interval:** ${formatSlowmode(newSeconds)}`,
    `**Previously:** ${formatSlowmode(oldSeconds)}`,
  ];
  if (reason) lines.push(`**Reason:** ${clampText(reason, 1000)}`);
  if (moderatorId) lines.push(`**Moderator:** <@${moderatorId}>`);
  if (forum) lines.push('*Applies to creating new posts in this forum.*');
  return successEmbed(title, lines.join('\n'));
}

// ── Reply helpers ──────────────────────────────────────────────────────────────

const NO_PINGS = { parse: [] };

async function attempt(label, fn) {
  try {
    return await fn();
  } catch (err) {
    if (!isInteractionGone(err)) logger.warn(`slowmode: ${label} failed`, err);
    return null;
  }
}

/** Replies by GuildSettings.modVisibility (D4). A failed public post never undoes the slowmode. */
async function deliverResult(interaction, visibility, args) {
  const withMod = buildSlowmodeEmbed(args);
  if (visibility === 'PRIVATE') {
    await attempt('private reply', () => interaction.editReply({ embeds: [withMod] }));
  } else if (visibility === 'ANONYMOUS') {
    await attempt('private confirmation', () => interaction.editReply({ embeds: [withMod] }));
    // An interaction follow-up shows who ran the command, so the anonymous notice is a plain channel message.
    const anonymous = buildSlowmodeEmbed({ ...args, moderatorId: null });
    await attempt('public notice', () => interaction.channel.send({ embeds: [anonymous], allowedMentions: NO_PINGS }));
  } else {
    await attempt('delete reply', () => interaction.deleteReply());
    await attempt('public follow-up', () => interaction.followUp({ embeds: [withMod], allowedMentions: NO_PINGS }));
  }
}

function editError(err, target, manageLabel) {
  if (err instanceof TimeoutError) {
    return errorEmbed('Still Waiting', 'Discord is rate-limiting changes to this channel. The change may still apply in a moment; check the channel settings before trying again.');
  }
  switch (err?.code) {
    case 50001:
    case 50013:
      return errorEmbed('Missing Permissions', `I couldn't change <#${target.id}>. Check that I have **${manageLabel}** there.`);
    case 10003:
      return errorEmbed('Channel Not Found', 'That channel no longer exists.');
    case 50083:
      return errorEmbed('Thread Archived', 'That thread is archived. Unarchive it, then try again.');
    case 50024:
    case 50035:
      return errorEmbed('Unsupported Channel', 'Discord rejected slowmode for this channel.');
    default:
      logger.error('slowmode edit failed', err);
      return errorEmbed('Slowmode Failed', `Discord returned an error${err?.code ? ` (code ${err.code})` : ''}. Please try again.`);
  }
}

// ── Command ────────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const guild = interaction.guild;
  if (!guild) return void await replyError(interaction, 'Server Only', 'Use this command in a server.');

  // 1. Duration.
  const rawDuration = interaction.options.getString('duration', true);
  const parsed = parseSlowmode(rawDuration);
  if (!parsed.ok) {
    if (parsed.code === 'too-long') {
      return void await replyError(interaction, 'Duration Too Long', 'Slowmode can be at most **6 hours** (Discord limit).');
    }
    if (parsed.code === 'too-short') {
      return void await replyError(interaction, 'Duration Too Short', 'Slowmode must be at least 1 second, or `off`.');
    }
    const clean = clampText(String(rawDuration).replace(/`/g, ''), 16);
    return void await replyError(interaction, 'Invalid Duration', `Couldn't read \`${clean}\`. Use a duration such as \`10s\`, \`5m\` or \`1h\` (max 6 hours), or \`off\`.`);
  }

  // 2. Target channel or thread (the lookup before the acknowledgement is bounded, rule 21).
  const picked = interaction.options.getChannel('channel');
  let target = interaction.channel;
  if (picked) {
    target = guild.channels.cache.get(picked.id)
      ?? await withTimeout(guild.channels.fetch(picked.id), ackBudgetMs(interaction), 'slowmode channel fetch').catch(() => null);
  }
  if (!target) {
    return void await replyError(interaction, 'Channel Not Found', "I can't see that channel. Pick one with the `channel` option.");
  }

  // 3. Type.
  if (target.type === ChannelType.GuildAnnouncement) {
    return void await replyError(interaction, 'Unsupported Channel', 'Announcement channels do not support slowmode.');
  }
  if (!SLOWMODE_TYPES.has(target.type)) {
    return void await replyError(interaction, 'Unsupported Channel', 'Slowmode can only be set on text, voice, stage, forum and media channels and on threads.');
  }

  // 4. Archived thread.
  const isThread = target.isThread?.() ?? false;
  if (isThread && target.archived) {
    return void await replyError(interaction, 'Thread Archived', 'That thread is archived. Unarchive it, then try again.');
  }

  // 5. Which permission a change needs (D2: threads need Manage Threads).
  const managePerm = isThread ? 'ManageThreads' : 'ManageChannels';
  const manageLabel = isThread ? 'Manage Threads' : 'Manage Channels';

  // 6. Executor (another channel only). The router has just loaded the settings, so this is normally a memo hit.
  const settings = peekSettings(interaction.guildId)
    ?? await withTimeout(getSettingsSafe(interaction.guildId), ackBudgetMs(interaction), 'slowmode settings').catch(() => null);
  if (target.id !== interaction.channelId && resolvePermissionLevel(interaction.member, settings) < PERMISSION_LEVELS.ADMIN) {
    const ok = target.permissionsFor(interaction.member)?.has([PermissionFlagsBits.ViewChannel, PermissionFlagsBits[managePerm]]);
    if (!ok) {
      return void await replyError(interaction, 'Missing Permissions', `You need **${manageLabel}** in <#${target.id}> to change its slowmode.`);
    }
  }

  // 7. Bot.
  if (!(await requireBotPermissions(interaction, ['ViewChannel', managePerm], { channel: target }))) return;

  // 8. No change.
  const oldSeconds = target.rateLimitPerUser ?? 0;
  if (oldSeconds === parsed.seconds) {
    const text = parsed.seconds === 0
      ? `Slowmode in <#${target.id}> is already off.`
      : `Slowmode in <#${target.id}> is already set to ${formatSlowmode(parsed.seconds)}.`;
    return void await safeReply(interaction, { embeds: [infoEmbed('No Change', text)], ephemeral: true });
  }

  // 9. Defer.
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;

  // 10. Apply.
  const reason = interaction.options.getString('reason')?.trim() || null;
  const auditReason = clampText(`${reason ?? 'No reason provided'} | by ${interaction.user.tag} via /slowmode`, 512);
  try {
    await withTimeout(target.edit({ rateLimitPerUser: parsed.seconds, reason: auditReason }), 15_000, 'slowmode edit');
  } catch (err) {
    await safeReply(interaction, { embeds: [editError(err, target, manageLabel)] });
    return;
  }

  // 11. Reply per modVisibility.
  const forum = target.type === ChannelType.GuildForum || target.type === ChannelType.GuildMedia;
  await deliverResult(interaction, settings?.modVisibility ?? 'PUBLIC', {
    channelId: target.id, oldSeconds, newSeconds: parsed.seconds, reason, moderatorId: interaction.user.id, forum,
  });
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

export async function autocomplete(interaction) {
  const v = interaction.options.getFocused()?.toString().trim() ?? '';
  const q = v.toLowerCase();
  const choices = [];
  if (v) {
    const p = parseSlowmode(v);
    if (p.ok && !PRESETS.some(([, value]) => value === q)) {
      choices.push({ name: `${formatSlowmode(p.seconds)} (custom)`, value: v.slice(0, 16) });
    }
    if (!p.ok && p.code === 'too-long') choices.push({ name: 'Too long: the maximum is 6 hours', value: '6h' });
  }
  for (const [name, value] of PRESETS) {
    if (!q || name.toLowerCase().includes(q) || value.includes(q)) choices.push({ name, value });
  }
  await respondAutocomplete(interaction, choices.slice(0, 25));
}
