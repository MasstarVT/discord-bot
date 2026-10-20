// Plugin `autotime`: answers times typed in chat ("fri 5pm", "tomorrow at 18:30", "7pm EST") with Discord timestamps,
// which every reader sees in their own timezone. A time is read in the author's zone: their own (/timezone set), else
// their timezone role (/settings timezone-role). Members with neither are answered only when the text names the zone,
// and are told (at most hourly) to pick a timezone role or use /timezone set; the server timezone is never assumed.
// Which text counts as a time: src/utils/timeMentions.js.
import { LIMITS } from '../config/constants.js';
import { missingBotPermissions } from '../utils/interactionHelpers.js';
import { createThrottle } from '../utils/async.js';
import { clampText } from '../utils/text.js';
import { discordTimestamp } from '../utils/time.js';
import { normalizeTimezoneRoles } from '../utils/timezones.js';
import { extractTimeSpans, mayMentionTime, readTimeSpans } from '../utils/timeMentions.js';
import logger from '../utils/logger.js';
import { peekSettings } from '../services/settingsService.js';
import { getUserTimezone, resolveMemberTimezone } from '../services/userTimezoneService.js';

export const AUTOTIME = Object.freeze({
  EMOJI: '🕒',
  // One answer per member per channel this often, so a burst of messages gets one reply.
  MEMBER_COOLDOWN_MS: 5_000,
  // The same instant is answered once per channel this often ("fri 5pm?" "yes fri 5pm").
  REPEAT_COOLDOWN_MS: 10 * 60_000,
  // A time that passed less than this long ago is still answered ("the raid started at 8pm").
  PAST_GRACE_MS: 60 * 60_000,
  // A member with no timezone is told how to set one once per server this often.
  ZONE_HINT_COOLDOWN_MS: 60 * 60_000,
});

const memberCooldown = createThrottle(AUTOTIME.MEMBER_COOLDOWN_MS);
const repeatCooldown = createThrottle(AUTOTIME.REPEAT_COOLDOWN_MS);
const zoneHintCooldown = createThrottle(AUTOTIME.ZONE_HINT_COOLDOWN_MS);

// Unknown channel or message, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES = new Set([10003, 10008, 50001, 50013]);

// Echoes typed text inside a code span: no span breakers, bounded.
const shown = (raw) => clampText(String(raw ?? '').replace(/[`\\]/g, '').trim(), 60) || '(time)';

/**
 * Whether times in this channel are never answered: the channel, its parent (a thread's channel or a channel's
 * category) or the parent's category is on GuildSettings.autoTimeIgnoredChannelIds.
 * @param {{ autoTimeIgnoredChannelIds?: string[] }|null|undefined} settings
 * @param {{ id?: string, parentId?: string|null, parent?: { parentId?: string|null }|null }|null|undefined} channel
 * @returns {boolean}
 */
export function isIgnoredChannel(settings, channel) {
  const list = settings?.autoTimeIgnoredChannelIds;
  if (!channel || !Array.isArray(list) || list.length === 0) return false;
  return [channel.id, channel.parentId, channel.parent?.parentId].some((id) => id != null && list.includes(id));
}

/**
 * Sync hook filter: the text may hold a time, and the memoised settings (when there are any) do not ignore the
 * channel. Runs on every message, so it never awaits.
 * @param {object} ctx HookContext with `message`
 * @returns {boolean}
 */
export function mayAnswer(ctx) {
  if (!mayMentionTime(ctx.message?.content)) return false;
  const settings = peekSettings(ctx.guildId);
  return !settings || !isIgnoredChannel(settings, ctx.message.channel);
}

/**
 * The reply: each time as a full date and a relative time, which Discord shows in every reader's own timezone.
 * The author's zone is not shown: a personal zone (/timezone set) is private.
 * @param {{ text: string, date: Date }[]} times
 * @returns {string}
 */
export function answerText(times) {
  const lines = times.map((t) => `${AUTOTIME.EMOJI} \`${shown(t.text)}\` → ${discordTimestamp(t.date, 'F')} (${discordTimestamp(t.date, 'R')})`);
  lines.push('-# Everyone sees this in their own timezone.');
  return lines.join('\n');
}

/**
 * The note for a member with no timezone: how to set one. Timezone roles are offered only when the server has some.
 * @param {string} text the typed time that could not be read
 * @param {boolean} hasRoles whether the server has timezone roles linked
 * @returns {string}
 */
export function zoneHintText(text, hasRoles) {
  const how = hasRoles ? 'Pick a timezone role, or save yours with `/timezone set`.' : 'Save yours with `/timezone set`.';
  return `⚠️ I can't convert \`${shown(text)}\` without your timezone. ${how}`;
}

/**
 * The author's own zone, when they have one: a saved zone or a timezone role. Null for the server timezone or UTC;
 * undefined when they have no timezone role and their saved zone could not be looked up.
 * @param {object} ctx HookContext with `message`
 * @param {object} settings
 * @returns {Promise<string|null|undefined>}
 */
export async function authorZone(ctx, settings) {
  const { message } = ctx;
  const personal = await getUserTimezone(message.author.id); // undefined when the lookup failed: the role still counts
  const resolved = resolveMemberTimezone({ member: message.member, guild: message.guild ?? ctx.guild, settings, personal: personal ?? null });
  if (resolved.source === 'personal' || resolved.source === 'role') return resolved.zone;
  return personal === undefined ? undefined : null;
}

const NOTHING = Object.freeze({ times: [], zoneHint: null });

/**
 * What to answer in a message, after the channel, zone and cooldown checks: the times, and the note on setting a
 * timezone when a time needed the author's zone and they have none.
 * @param {object} ctx HookContext with `message`
 * @param {Date} now
 * @returns {Promise<{ times: { text: string, date: Date, zone: string, zoneFromInput: boolean }[], zoneHint: string|null }>}
 */
async function readMessage(ctx, now) {
  const { message } = ctx;
  const settings = await ctx.getSettings();
  if (!settings || isIgnoredChannel(settings, message.channel)) return NOTHING;
  const spans = extractTimeSpans(message.content);
  if (!spans.length) return NOTHING;
  // The zone lookup only happens once the text holds a time, and not at all when every time names its zone.
  const unzoned = spans.find((s) => !s.zone);
  const zone = unzoned ? await authorZone(ctx, settings) : null;
  const times = readTimeSpans(spans, zone, { now, pastGraceMs: AUTOTIME.PAST_GRACE_MS, max: LIMITS.AUTOTIME_PER_MESSAGE });
  const noZone = Boolean(unzoned) && zone === null; // undefined: the lookup failed, so they may well have one
  if ((!times.length && !noZone) || !memberCooldown(`${message.channelId}:${message.author.id}`)) return NOTHING;
  const guild = message.guild ?? ctx.guild;
  const hasRoles = normalizeTimezoneRoles(settings.timezoneRoles).some((l) => guild?.roles?.cache?.has(l.roleId));
  return {
    times: times.filter((t) => repeatCooldown(`${message.channelId}:${t.date.getTime()}`)),
    zoneHint: noZone && zoneHintCooldown(`${message.guildId}:${message.author.id}`) ? zoneHintText(unzoned.text, hasRoles) : null,
  };
}

/**
 * The times to answer in a message, after the channel, zone and cooldown checks; empty for none.
 * @param {object} ctx HookContext with `message`
 * @param {{ now?: Date }} [opts]
 * @returns {Promise<{ text: string, date: Date, zone: string, zoneFromInput: boolean }[]>}
 */
export async function timesToAnswer(ctx, { now = new Date() } = {}) {
  return (await readMessage(ctx, now)).times;
}

/** Replies to the message without pinging anyone; never throws. */
async function reply(message, content) {
  const channel = message.channel;
  const send = channel?.isThread?.() ? 'SendMessagesInThreads' : 'SendMessages';
  if (missingBotPermissions(channel, ['ViewChannel', send, 'ReadMessageHistory']).length) return;
  try {
    await message.reply({ content, allowedMentions: { parse: [], repliedUser: false }, failIfNotExists: false });
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[autotime] ${message.guildId}: could not reply: ${err?.message}`);
  }
}

/** Answers the times in one message, if any, and tells a member with no timezone how to set one. */
export async function handleMessage(ctx) {
  const { times, zoneHint } = await readMessage(ctx, new Date());
  const content = [times.length ? answerText(times) : null, zoneHint].filter(Boolean).join('\n');
  if (content) await reply(ctx.message, content);
}

export const plugin = {
  id: 'autotime',
  module: 'autotime',
  hooks: [
    {
      event: 'messageCreate',
      id: 'autotime', // HOOK_ORDER.messageCreate: last, after automod has removed what it removes
      filter: mayAnswer,
      run: handleMessage,
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
