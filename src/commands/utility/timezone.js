// /timezone: a member's own timezone, saved once for every server. /timestamp reads times in it,
// ahead of the server's timezone roles (/settings timezone-role) and the server timezone.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import {
  getUserTimezone, setUserTimezone, clearUserTimezone, peekUserTimezone, resolveMemberTimezone,
} from '../../services/userTimezoneService.js';
import { successEmbed, errorEmbed, infoEmbed } from '../../utils/embedBuilder.js';
import {
  parseZoneInput, zoneInputError, searchTimeZones, describeLocalTime, zoneLabel, zoneSourceText, normalizeTimezoneRoles,
} from '../../utils/timezones.js';
import { safeDefer, safeReply, replyError, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = null; // core command: never module-gated
export const permissionLevel = PERMISSION_LEVELS.MEMBER;

export const data = new SlashCommandBuilder()
  .setName('timezone')
  .setDescription('Save your timezone so /timestamp reads your times right in every server.')
  .addSubcommand((s) => s.setName('set')
    .setDescription('Save your timezone. It beats timezone roles and the server timezone.')
    .addStringOption((o) => o.setName('zone')
      .setDescription('Region/City (America/Chicago) or an abbreviation like CST. Type an offset like -6 to search.')
      .setRequired(true).setMaxLength(64).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('show')
    .setDescription('Show which timezone /timestamp uses for you, and why.'))
  .addSubcommand((s) => s.setName('clear')
    .setDescription('Forget your saved timezone; your timezone role or the server timezone is used again.'));

const reply = (embeds) => ({ embeds, allowedMentions: { parse: [] } });

/** "It's 15:12 on Friday there" for a zone. */
const localNow = (zone) => {
  const lt = describeLocalTime(zone);
  return `It's **${lt.time}** on ${lt.weekday}, ${lt.date} there now.`;
};

// What /timestamp would use for this member, after a change to their own zone.
async function resolveFor(interaction, personal) {
  const settings = interaction.guildId ? await getSettingsSafe(interaction.guildId) : null;
  return { settings, resolved: resolveMemberTimezone({ member: interaction.member, guild: interaction.guild, settings, personal }) };
}

async function runSet(interaction) {
  const raw = interaction.options.getString('zone', true);
  const r = parseZoneInput(raw);
  if (!r.ok) return replyError(interaction, ...zoneInputError(raw, r));
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  try {
    await setUserTimezone(interaction.user.id, r.zone);
  } catch (err) {
    logger.error('[timezone] save failed', err, { userId: interaction.user.id });
    return safeReply(interaction, reply([errorEmbed('Could Not Save', 'Your timezone could not be saved. Please try again in a moment.')]));
  }
  logger.info(`[timezone] ${interaction.user.id} → ${r.zone}`);
  const lines = [
    `Your timezone is now **${zoneLabel(r.zone)}**.`,
    localNow(r.zone),
    '',
    '/timestamp reads your times in it in every server, ahead of timezone roles and the server timezone.',
  ];
  if (r.abbreviation) lines.push(`${r.abbreviation} is read as ${r.zone} time, so it follows daylight saving.`);
  return safeReply(interaction, reply([successEmbed('Timezone Saved', lines.join('\n'))]));
}

async function runShow(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  const personal = await getUserTimezone(interaction.user.id);
  const { settings, resolved } = await resolveFor(interaction, personal ?? null);
  const link = normalizeTimezoneRoles(settings?.timezoneRoles).find((l) => l.roleId === resolved.roleId);
  const saved = personal === undefined ? 'could not be loaded right now' : personal ? `\`${personal}\`` : 'none (`/timezone set`)';
  const lines = [
    `/timestamp reads your times in **${zoneLabel(resolved.zone)}**, from ${zoneSourceText(resolved)}.`,
    localNow(resolved.zone),
    '',
    `• Saved timezone: ${saved}`,
    `• Timezone role: ${link ? `<@&${link.roleId}> → \`${link.zone}\`${resolved.roleCount > 1 ? ' (your highest of several)' : ''}` : 'none'}`,
    `• Server timezone: \`${settings?.timezone ?? 'UTC'}\``,
  ];
  return safeReply(interaction, reply([infoEmbed('Your Timezone', lines.join('\n'))]));
}

async function runClear(interaction) {
  if (!(await safeDefer(interaction, { ephemeral: true }))) return;
  let cleared;
  try {
    cleared = await clearUserTimezone(interaction.user.id);
  } catch (err) {
    logger.error('[timezone] clear failed', err, { userId: interaction.user.id });
    return safeReply(interaction, reply([errorEmbed('Could Not Clear', 'Your timezone could not be cleared. Please try again in a moment.')]));
  }
  const { resolved } = await resolveFor(interaction, null);
  const now = `/timestamp uses **${zoneLabel(resolved.zone)}**, from ${zoneSourceText(resolved)}.`;
  return safeReply(interaction, reply([cleared
    ? successEmbed('Timezone Cleared', `Your saved timezone is gone. ${now}`)
    : infoEmbed('No Saved Timezone', `You had no saved timezone. ${now}`)]));
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'set') return runSet(interaction);
  if (sub === 'show') return runShow(interaction);
  if (sub === 'clear') return runClear(interaction);
}

export async function autocomplete(interaction) {
  let choices = [];
  try {
    const focused = interaction.options.getFocused(true);
    if (focused.name === 'zone') {
      choices = searchTimeZones(focused.value, { current: peekUserTimezone(interaction.user.id) ?? null, abbreviations: true });
    }
  } catch (err) {
    logger.warn(`[timezone] autocomplete failed: ${err?.message}`);
    choices = [];
  }
  await respondAutocomplete(interaction, choices); // clamps to 25 / 100 chars and catches
}
