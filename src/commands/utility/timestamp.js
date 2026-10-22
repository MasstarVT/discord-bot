// /timestamp: turns a typed time ("fri 7pm", "tomorrow 18:30", "7pm CST") into Discord timestamps,
// which every reader sees in their own timezone. The text is read in the caller's zone: their own
// (/timezone set), else their timezone role (/settings timezone-role), else the server timezone.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe, peekSettings } from '../../services/settingsService.js';
import { getUserTimezone, resolveMemberTimezone } from '../../services/userTimezoneService.js';
import { guildEmbed } from '../../utils/embedBuilder.js';
import { parseWhen, discordTimestamp, zonedParts } from '../../utils/time.js';
import { formatInZone, zoneLabel, zoneSourceText } from '../../utils/timezones.js';
import { withTimeout, ackBudgetMs } from '../../utils/async.js';
import { clampText } from '../../utils/text.js';
import { safeReply, replyError, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = null; // core command: never module-gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

// Discord's timestamp styles, in the order the reply lists them.
export const STYLES = Object.freeze([
  { value: 'F', name: 'Long date and time (Friday, October 16, 2026 7:00 PM)' },
  { value: 'f', name: 'Date and time (October 16, 2026 7:00 PM)' },
  { value: 'D', name: 'Long date (October 16, 2026)' },
  { value: 'd', name: 'Short date (10/16/2026)' },
  { value: 't', name: 'Time (7:00 PM)' },
  { value: 'T', name: 'Time with seconds (7:00:00 PM)' },
  { value: 'R', name: 'Relative (in 2 hours)' },
]);

export const EXAMPLES = Object.freeze(['now', 'in 2h', 'tomorrow 12pm', 'fri 7pm', 'next mon 9am']);

const MONTH_NAMES = Object.freeze(['jan', 'feb', 'mar', 'apr', 'may', 'jun', 'jul', 'aug', 'sep', 'oct', 'nov', 'dec']);

/**
 * A calendar day a week from now in `zone`, as a hint would type it ("oct 29") and as an ISO date. Always ahead, so a
 * hint never shows a day that has already passed.
 * @param {string} zone
 * @param {Date} [now]
 * @returns {{ words: string, iso: string }}
 */
export function futureDayExample(zone, now = new Date()) {
  const p = zonedParts(now, zone);
  const d = new Date(Date.UTC(p.year, p.month - 1, p.day + 7));
  const month = d.getUTCMonth();
  const day = d.getUTCDate();
  const pad = (n) => String(n).padStart(2, '0');
  return { words: `${MONTH_NAMES[month]} ${day}`, iso: `${d.getUTCFullYear()}-${pad(month + 1)}-${pad(day)}` };
}

export const data = new SlashCommandBuilder()
  .setName('timestamp')
  .setDescription('Turn a time into a timestamp that everyone sees in their own timezone.')
  .addStringOption((o) => o.setName('when')
    .setDescription('e.g. fri 7pm, tomorrow 18:30, oct 20 9am, in 2h, 7pm CST')
    .setRequired(true).setMaxLength(100).setAutocomplete(true))
  .addStringOption((o) => o.setName('style')
    .setDescription('How the posted timestamp looks (with public: True). Default: long date and time.')
    .addChoices(...STYLES.map(({ name, value }) => ({ name, value }))))
  .addBooleanOption((o) => o.setName('public')
    .setDescription('True: post the timestamp in this channel. False (default): only you see the codes.'));

// ── Helpers ──────────────────────────────────────────────────────────────────

// Echoes user input inside a code span: no span breakers, bounded.
const shown = (raw) => clampText(String(raw ?? '').replace(/[`\\]/g, '').trim(), 64) || '(empty)';

/**
 * Rough distance from now for autocomplete names: 'in 3 hours', '2 days ago', 'now'.
 * @param {number} ms - target minus now
 * @returns {string}
 */
export function roughRelative(ms) {
  const abs = Math.abs(ms);
  const units = [[31_536_000_000, 'year'], [2_592_000_000, 'month'], [86_400_000, 'day'], [3_600_000, 'hour'], [60_000, 'minute']];
  for (const [size, unit] of units) {
    if (abs >= size) {
      const n = Math.round(abs / size);
      const text = `${n} ${unit}${n === 1 ? '' : 's'}`;
      return ms > 0 ? `in ${text}` : `${text} ago`;
    }
  }
  return 'now';
}

/**
 * Autocomplete choices for `when`: a preview of how the typed text is read, or examples when empty.
 * The value is always the text itself, so execute reads it again.
 * @param {unknown} typed
 * @param {string} zone
 * @param {Date} [now]
 * @returns {{ name: string, value: string }[]}
 */
export function suggestWhen(typed, zone, now = new Date()) {
  const q = String(typed ?? '').trim().slice(0, 100);
  const out = [];
  for (const input of q ? [q] : EXAMPLES) {
    const r = parseWhen(input, zone, { now });
    if (!r) continue;
    const preview = `${formatInZone(r.date, r.zone)} · ${zoneLabel(r.zone, r.date)} · ${roughRelative(r.date.getTime() - now.getTime())}`;
    out.push({ name: q ? preview : `${input} → ${preview}`, value: input });
  }
  if (q && !out.length) {
    out.push({ name: `Can't read "${clampText(q, 30)}" yet. Try: fri 7pm · tomorrow 18:30 · ${futureDayExample(zone, now).words} 9am · in 2h`, value: q });
  }
  return out;
}

/**
 * The private reply: how the text was read, then every style as a copyable code and its preview.
 * @param {{ date: Date, zone: string, zoneFromInput: boolean }} parsed
 * @param {{ zone: string, source: string, roleId: string|null, roleCount: number }} resolved
 * @param {{ style?: string, personalUnknown?: boolean, settings?: object|null }} [opts]
 */
export function buildTimestampEmbed(parsed, resolved, { style = 'F', personalUnknown = false, settings = null } = {}) {
  const order = [style, ...STYLES.map((s) => s.value).filter((v) => v !== style)];
  const lines = [
    `**${formatInZone(parsed.date, parsed.zone)}** · ${zoneLabel(parsed.zone, parsed.date)}`,
    `Read in ${zoneSourceText(resolved, parsed)}. Everyone sees these in their own timezone.`,
  ];
  if (!parsed.zoneFromInput && resolved.source === 'role' && resolved.roleCount > 1) {
    lines.push('You have several timezone roles, so the highest one was used.');
  }
  if (!parsed.zoneFromInput && personalUnknown) {
    lines.push('⚠️ Your saved timezone could not be loaded just now, so this one was used.');
  }
  lines.push('', 'Copy a code into any message:');
  for (const st of order) {
    const code = discordTimestamp(parsed.date, st);
    lines.push(`\`${code}\` ${code}`);
  }
  if (!parsed.zoneFromInput && (resolved.source === 'server' || resolved.source === 'default')) {
    lines.push('', 'Not your timezone? Save yours with `/timezone set`, or add one to the time, e.g. `7pm CST`.');
  }
  return guildEmbed(settings, '🕒 Timestamp', lines.join('\n'));
}

/**
 * The caller's zone. Lookups are bounded by the 3 s ack budget; a slow or failed one falls back
 * to the next source (personalUnknown says so).
 * @param {import('discord.js').BaseInteraction} interaction
 * @param {{ cacheOnly?: boolean, budgetMs?: number }} [opts]
 */
export async function loadCallerZone(interaction, { cacheOnly = false, budgetMs = ackBudgetMs(interaction) } = {}) {
  const { guildId } = interaction;
  const [settings, personal] = await Promise.all([
    guildId
      ? (peekSettings(guildId) ?? withTimeout(getSettingsSafe(guildId), budgetMs, 'timestamp settings').catch(() => null))
      : null,
    withTimeout(getUserTimezone(interaction.user.id, { cacheOnly }), budgetMs, 'user timezone').catch(() => undefined),
  ]);
  const resolved = resolveMemberTimezone({ member: interaction.member, guild: interaction.guild, settings, personal: personal ?? null });
  return { resolved, settings, personalUnknown: personal === undefined && !cacheOnly };
}

// ── Handlers ─────────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const raw = interaction.options.getString('when', true);
  const style = interaction.options.getString('style') ?? 'F';
  const isPublic = interaction.options.getBoolean('public') ?? false;

  const { resolved, settings, personalUnknown } = await loadCallerZone(interaction);
  const parsed = parseWhen(raw, resolved.zone);
  if (!parsed) {
    const day = futureDayExample(resolved.zone);
    return replyError(interaction, 'Could Not Read That Time', [
      `I couldn't read \`${shown(raw)}\` as a time.`,
      `Try \`fri 7pm\`, \`tomorrow 18:30\`, \`next mon 9am\`, \`${day.words} 7:30pm\`, \`${day.iso} 19:00\`, \`in 2h\` or \`7pm CST\`.`,
      `Times are read in **${zoneLabel(resolved.zone)}**, ${zoneSourceText(resolved)}.`,
    ].join('\n'));
  }

  if (isPublic) {
    const main = discordTimestamp(parsed.date, style);
    const content = style === 'R' ? `🕒 ${main}` : `🕒 ${main} (${discordTimestamp(parsed.date, 'R')})`;
    return safeReply(interaction, { content, allowedMentions: { parse: [] } });
  }
  return safeReply(interaction, {
    embeds: [buildTimestampEmbed(parsed, resolved, { style, personalUnknown, settings })],
    ephemeral: true,
    allowedMentions: { parse: [] },
  });
}

export async function autocomplete(interaction) {
  let choices = [];
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name === 'when') {
      const { resolved } = await loadCallerZone(interaction, { cacheOnly: true, budgetMs: Math.min(1000, ackBudgetMs(interaction)) });
      choices = suggestWhen(focused.value, resolved.zone);
    }
  } catch (err) {
    logger.warn(`[timestamp] autocomplete failed: ${err?.message}`);
    choices = [];
  }
  await respondAutocomplete(interaction, choices);
}
