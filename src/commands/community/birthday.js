// /birthday (pkg-birthdays): MEE6 /remember-birthdays, /forget-birthday, /birthday and /next-birthdays as subcommands. This file only
// reads options and formats replies; the behaviour lives in services/birthdayService.js.
// `show` and `next` are public and show month and day only (never the year or the age); `set` and `forget` answer privately.
import { SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  NEXT_LIMIT, formatBirthday, formatHour, clampHour, getBirthday, getUpcomingBirthdays, nextOccurrence, parseBirthdayInput,
  removeBirthday, resolveTimeZone, saveBirthday,
} from '../../services/birthdayService.js';
import { getSettingsSafe } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import { safeDefer, safeReply } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { clampText } from '../../utils/text.js';
import { discordTimestamp, zonedParts, zonedToUtc } from '../../utils/time.js';

export const module = 'birthdays';
export const gated = true;
export const permissionLevel = PERMISSION_LEVELS.MEMBER;
// No allowInDMs export: buildCommandPayload registers the command for servers only.

// ── Builder ───────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('birthday')
  .setDescription('Share and see birthdays (MEE6 /remember-birthdays, /birthday, /next-birthdays).')
  .addSubcommand((s) => s.setName('set')
    .setDescription('Remember your birthday on this server (MEE6 /remember-birthdays).')
    .addStringOption((o) => o.setName('date')
      .setDescription('MM-DD or YYYY-MM-DD, e.g. 03-14 or 1998-03-14')
      .setRequired(true).setMinLength(3).setMaxLength(10)))
  .addSubcommand((s) => s.setName('forget')
    .setDescription('Forget your birthday on this server (MEE6 /forget-birthday).'))
  .addSubcommand((s) => s.setName('show')
    .setDescription("Show your or a member's birthday (MEE6 /birthday).")
    .addUserOption((o) => o.setName('member').setDescription('Member to look up (default: you)')))
  .addSubcommand((s) => s.setName('next')
    .setDescription('List the next 10 birthdays on this server (MEE6 /next-birthdays).'));

// ── Messages (pkg-birthdays.md §4.3) ──────────────────────────────────────────

const U1 = () => errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const G1 = () => errorEmbed('Something Went Wrong', 'That did not work. Please try again in a moment.');
const E1 = () => errorEmbed('Not a Member', 'That user is not a member of this server.');

/** Every reply: embeds only, and nothing pings. */
const send = (interaction, embed) => safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });

// ── Subcommands ───────────────────────────────────────────────────────────────

async function handleSet(interaction) {
  const { guild } = interaction;
  const settings = await getSettingsSafe(guild.id);
  if (!settings) return send(interaction, U1());

  const tz = resolveTimeZone(settings);
  const now = new Date();
  const r = parseBirthdayInput(interaction.options.getString('date', true), {
    requireYear: settings.birthdayRequireYear === true, now, tz,
  });
  if (!r.ok) return send(interaction, errorEmbed('Invalid Date', r.message));

  const res = await saveBirthday({ guild, userId: interaction.user.id, date: r, settings, now });
  const long = formatBirthday(r, { withYear: true });
  const hh = formatHour(clampHour(settings.birthdayHour));

  const lines = [];
  if (res.late) {
    lines.push(`Happy birthday! 🎉 Today's birthdays were celebrated at **${hh}**, so I'll celebrate you from next year.`);
  } else if (res.alreadyCelebrated) {
    lines.push('You were already celebrated this year, so the new date counts from next year.');
  }
  if (!settings.birthdayChannelId) lines.push("Birthday announcements aren't set up on this server yet.");
  else if (!res.late && !res.alreadyCelebrated) lines.push(`It will be announced in <#${settings.birthdayChannelId}> at **${hh}** (${tz}).`);
  if (r.year !== null && settings.birthdayChannelId) lines.push('Your age will be included in the announcement.');

  const head = res.created ? `I'll celebrate your birthday on **${long}**.` : `Your birthday is now **${long}**.`;
  const body = lines.length ? `${head}\n\n${lines.join('\n')}` : head;
  return send(interaction, successEmbed(res.created ? 'Birthday Saved' : 'Birthday Updated', body));
}

async function handleForget(interaction) {
  const settings = await getSettingsSafe(interaction.guild.id); // null is fine: the role removal is skipped, the deletion still happens
  const res = await removeBirthday({ guild: interaction.guild, userId: interaction.user.id, settings });
  return send(interaction, res.removed
    ? successEmbed('Birthday Forgotten', "I've removed your birthday from this server.")
    : infoEmbed('Nothing to Forget', "You haven't shared your birthday on this server."));
}

async function handleShow(interaction) {
  const { guild } = interaction;
  const user = interaction.options.getUser('member') ?? interaction.user;
  const member = interaction.options.getMember('member') ?? (user.id === interaction.user.id ? interaction.member : null);
  if (!member) return send(interaction, E1());

  const row = user.bot ? null : await getBirthday(guild.id, user.id);
  if (!row) {
    return send(interaction, user.id === interaction.user.id
      ? infoEmbed('No Birthday Saved', "You haven't shared your birthday yet. Add it with `/birthday set`.")
      : infoEmbed('No Birthday Saved', `${member} hasn't shared their birthday.`));
  }

  const settings = await getSettingsSafe(guild.id); // null: UTC and the default colour
  const tz = resolveTimeZone(settings);
  const local = zonedParts(new Date(), tz);
  const occ = nextOccurrence(row, local);
  const isToday = occ.year === local.year && occ.month === local.month && occ.day === local.day;
  const at = zonedToUtc({ year: occ.year, month: occ.month, day: occ.day }, tz);

  // Never the year or the age (privacy): only the announcement may reveal them.
  let desc = `${member}'s birthday is **${formatBirthday(row)}**.`;
  desc += isToday ? "\n🎉 It's today!" : `\nNext: ${discordTimestamp(at, 'D')} (${discordTimestamp(at, 'R')})`;
  return send(interaction, guildEmbed(settings, '🎂 Birthday', desc, `Server time zone: ${tz}`));
}

async function handleNext(interaction) {
  const settings = await getSettingsSafe(interaction.guild.id);
  const tz = resolveTimeZone(settings);
  const list = await getUpcomingBirthdays(interaction.guild, settings, { limit: NEXT_LIMIT, now: new Date() });
  if (list.length === 0) {
    return send(interaction, infoEmbed('No Birthdays Yet', 'Nobody has shared a birthday yet. Add yours with `/birthday set`.'));
  }
  const lines = list.map((e) => (e.isToday
    ? `🎉 **Today** — <@${e.userId}>`
    : `**${formatBirthday(e, { short: true })}** — <@${e.userId}> · ${discordTimestamp(e.at, 'R')}`));
  return send(interaction, guildEmbed(settings, '🎂 Upcoming Birthdays', clampText(lines.join('\n'), 4096), `Server time zone: ${tz}`));
}

const HANDLERS = Object.freeze({ set: handleSet, forget: handleForget, show: handleShow, next: handleNext });

// ── Entry point ───────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  const handler = HANDLERS[sub];
  if (!handler) return;
  // set and forget answer privately; show and next are public. The acknowledgement comes first (00-overview rule 21).
  if (!(await safeDefer(interaction, { ephemeral: sub === 'set' || sub === 'forget' }))) return;
  try {
    await handler(interaction);
  } catch (err) {
    logger.error(`birthday ${sub} failed`, err);
    await send(interaction, G1());
  }
}
