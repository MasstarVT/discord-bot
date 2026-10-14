// Plugin `counting`: the counting game in GuildSettings.countingChannelId. A message that is only a number counts: the
// next number from a different member gets ✅, anything else gets ❌ and, once the count has started, resets it to 0 with
// a note in the channel. Other messages are chat and are ignored. Rules and state: src/services/countingService.js.
import { errorEmbed, infoEmbed } from '../utils/embedBuilder.js';
import { missingBotPermissions } from '../utils/interactionHelpers.js';
import { createThrottle } from '../utils/async.js';
import { formatNumber } from '../utils/text.js';
import logger from '../utils/logger.js';
import {
  runLocked, parseCount, currentState, judgeCount, loadState, saveState,
} from '../services/countingService.js';

export const COUNTING = Object.freeze({
  OK_EMOJI: '✅',
  FAIL_EMOJI: '❌',
  // A wrong first number has nothing to reset, so its "start at 1" hint is posted at most this often per channel.
  START_HINT_EVERY_MS: 60_000,
});

const startHint = createThrottle(COUNTING.START_HINT_EVERY_MS);

// Unknown channel or message, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES = new Set([10003, 10008, 50001, 50013]);

/** Cheap, synchronous pre-filter: the message starts with a digit (the full parse happens later). */
export function looksLikeCount(message) {
  const c = message?.content;
  return typeof c === 'string' && /^\s*\d/.test(c);
}

/** The note posted when a run ends. */
export function ruinedText({ userId, count, reason, expected, best }) {
  const why = reason === 'twice' ? "You can't count twice in a row." : `The next number was **${formatNumber(expected)}**.`;
  return `<@${userId}> ruined it at **${formatNumber(count)}**! ${why}\n`
    + `Start again from **1**. Best run: **${formatNumber(best)}**.`;
}

/** The note posted when the last counted message is deleted. */
export function deletedText({ userId, count }) {
  return `${userId ? `<@${userId}>` : 'Someone'} deleted their number **${formatNumber(count)}**. `
    + `The next number is **${formatNumber(count + 1)}**.`;
}

async function react(message, emoji) {
  if (missingBotPermissions(message.channel, ['AddReactions', 'ReadMessageHistory']).length) return;
  try {
    await message.react(emoji);
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[counting] ${message.guildId}: could not react: ${err?.message}`);
  }
}

/** Posts a bot note in the counting channel; never pings and never throws. */
export async function postNote(channel, embed) {
  if (missingBotPermissions(channel, ['ViewChannel', 'SendMessages', 'EmbedLinks']).length) return;
  try {
    await channel.send({ embeds: [embed], allowedMentions: { parse: [] } });
  } catch (err) {
    if (!GONE_CODES.has(err?.code)) logger.warn(`[counting] ${channel.guildId}: could not post: ${err?.message}`);
  }
}

/** Judges one message in the counting channel. Runs under the guild's lock. */
export async function handleCount(ctx) {
  const { message, guildId } = ctx;
  const settings = await ctx.getSettings();
  const channelId = settings?.countingChannelId;
  if (!channelId || message.channelId !== channelId) return;
  const number = parseCount(message.content);
  if (number === null) return;

  const state = currentState(await loadState(guildId), channelId);
  const verdict = judgeCount(state, message.author.id, number);

  if (verdict.ok) {
    await saveState(guildId, {
      channelId, count: number, lastUserId: message.author.id, lastMessageId: message.id, best: Math.max(state.best, number),
    });
    await react(message, COUNTING.OK_EMOJI);
    return;
  }

  if (state.count === 0) {
    // Nothing to lose yet: no reset and no public shaming, just a hint now and then.
    await react(message, COUNTING.FAIL_EMOJI);
    if (startHint(channelId)) await postNote(message.channel, infoEmbed('Counting', 'The count starts at **1**.'));
    return;
  }

  await saveState(guildId, { channelId, count: 0, lastUserId: null, lastMessageId: null, best: state.best });
  await react(message, COUNTING.FAIL_EMOJI);
  await postNote(message.channel, errorEmbed('Count Ruined', ruinedText({
    userId: message.author.id, count: state.count, reason: verdict.reason, expected: verdict.expected, best: state.best,
  })));
}

/** Tells the channel the next number when the message holding the current count is deleted. */
export async function handleDelete(ctx) {
  const { message, guildId } = ctx;
  const settings = await ctx.getSettings();
  const channelId = settings?.countingChannelId;
  if (!channelId || message.channelId !== channelId) return;

  const row = await loadState(guildId);
  if (!row || row.channelId !== channelId || row.count === 0 || row.lastMessageId !== message.id) return;
  const channel = message.channel ?? ctx.guild.channels.cache.get(channelId);
  if (channel) await postNote(channel, infoEmbed('Number Deleted', deletedText({ userId: row.lastUserId, count: row.count })));
}

export const plugin = {
  id: 'counting',
  module: 'counting',
  hooks: [
    {
      event: 'messageCreate',
      id: 'counting', // HOOK_ORDER.messageCreate: right after automod, so counts keep their gateway order
      filter: (ctx) => looksLikeCount(ctx.message),
      // Take the lock before any await: the order messages enter it is the order they are judged.
      run: (ctx) => runLocked(ctx.guildId, () => handleCount(ctx)),
    },
    {
      event: 'messageDelete',
      id: 'counting',
      run: (ctx) => runLocked(ctx.guildId, () => handleDelete(ctx)),
    },
  ],
  components: [],
  jobs: [],
  provides: {},
};
