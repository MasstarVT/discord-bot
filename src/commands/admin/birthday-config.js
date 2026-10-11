// /birthday-config (pkg-birthdays): the admin side of MEE6 Birthdays: announcement channel, birthday role, announcement hour, the two
// messages, the birth-year requirement, MEE6 /set-user-birthday and /unset-user-birthday, plus `show` and `test`. Settings are written
// only through settingsService; the behaviour (scheduling, the role transfer, saving birthdays) lives in services/birthdayService.js.
// `gated = false`: the command works while the module is off (replies then carry a note). Replies are public (admin convention) except
// `test`, which is a private preview.
import { ChannelType, PermissionFlagsBits, SlashCommandBuilder } from 'discord.js';
import { PERMISSION_LEVELS } from '../../config/constants.js';
import {
  DEFAULT_BIRTHDAY_MESSAGE, DEFAULT_BIRTHDAY_MESSAGE_AGE, codeBlock, countBirthdays, ensureTickJob, formatBirthday,
  formatHour, parseBirthdayInput, pickTemplate, removeBirthday, renderBirthdayLine, resolveTimeZone, saveBirthday, tickKey,
  transferBirthdayRole,
} from '../../services/birthdayService.js';
import { getJob } from '../../services/scheduler.js';
import { getSettingsSafe, isModuleEnabled, updateSettings } from '../../services/settingsService.js';
import { errorEmbed, guildEmbed, infoEmbed, successEmbed } from '../../utils/embedBuilder.js';
import {
  checkAssignableRole, checkChannelUsable, permissionNames, safeDefer, safeReply,
} from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';
import { renderPlaceholdersDetailed } from '../../utils/placeholderParser.js';
import { clampText, formatNumber } from '../../utils/text.js';
import { discordTimestamp, zonedParts } from '../../utils/time.js';

export const module = 'birthdays';
export const gated = false; // works while the module is off
export const permissionLevel = PERMISSION_LEVELS.ADMIN; // every subcommand

// ── Builder ───────────────────────────────────────────────────────────────────

export const data = new SlashCommandBuilder()
  .setName('birthday-config')
  .setDescription('Configure birthday announcements and the birthday role (MEE6 Birthdays).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('channel')
    .setDescription('Set the birthday announcement channel; leave empty to turn announcements off.')
    .addChannelOption((o) => o.setName('channel').setDescription('Text or announcement channel (empty = off)')
      .addChannelTypes(ChannelType.GuildText, ChannelType.GuildAnnouncement)))
  .addSubcommand((s) => s.setName('role')
    .setDescription('Set the role members get on their birthday; leave empty to turn it off.')
    .addRoleOption((o) => o.setName('role').setDescription('Birthday role (empty = off)')))
  .addSubcommand((s) => s.setName('hour')
    .setDescription('Set the hour (server timezone) when birthdays are announced.')
    .addIntegerOption((o) => o.setName('hour').setDescription('0–23, e.g. 9 = 09:00 (default 9)')
      .setRequired(true).setMinValue(0).setMaxValue(23)))
  .addSubcommand((s) => s.setName('message')
    .setDescription('Set the message used when the age is unknown; leave empty for the default.')
    .addStringOption((o) => o.setName('text').setDescription('Supports {user}, {user.name}, {server} and more')
      .setMaxLength(1000)))
  .addSubcommand((s) => s.setName('message-age')
    .setDescription('Set the message used when the age is known; leave empty for the default.')
    .addStringOption((o) => o.setName('text').setDescription('Supports {user}, {age}, {server} and more')
      .setMaxLength(1000)))
  .addSubcommand((s) => s.setName('require-year')
    .setDescription('Require members to include their birth year.')
    .addBooleanOption((o) => o.setName('enabled').setDescription('true = members must use YYYY-MM-DD').setRequired(true)))
  .addSubcommand((s) => s.setName('set-user')
    .setDescription("Set a member's birthday (MEE6 /set-user-birthday).")
    .addUserOption((o) => o.setName('member').setDescription('The member').setRequired(true))
    .addStringOption((o) => o.setName('date').setDescription('MM-DD or YYYY-MM-DD').setRequired(true).setMinLength(3).setMaxLength(10)))
  .addSubcommand((s) => s.setName('unset-user')
    .setDescription("Remove a member's birthday (MEE6 /unset-user-birthday).")
    .addUserOption((o) => o.setName('member').setDescription('The member').setRequired(true)))
  .addSubcommand((s) => s.setName('show').setDescription('Show the birthday settings and any problems.'))
  .addSubcommand((s) => s.setName('test').setDescription('Preview both birthday messages with you as the birthday member.'));

// ── Messages (pkg-birthdays.md §4.3) ──────────────────────────────────────────

const U1 = () => errorEmbed('Temporarily Unavailable', 'Server settings could not be loaded. Please try again in a moment.');
const G1 = () => errorEmbed('Something Went Wrong', 'That did not work. Please try again in a moment.');
const G2 = () => errorEmbed('Could Not Save', 'Something went wrong while saving. Please try again in a moment.');
const E1 = () => errorEmbed('Not a Member', 'That user is not a member of this server.');
const E2 = () => errorEmbed('Not Allowed', "Bots don't have birthdays.");
const N1 = '\n\n⚠️ The **Birthdays** module is off. Turn it on with `/module enable birthdays`.';

/** Every reply: embeds only, and nothing pings. */
const send = (interaction, embed) => safeReply(interaction, { embeds: [embed], allowedMentions: { parse: [] } });

/** A database write failed (answered with G2); keeps the original error for the log. */
class WriteError extends Error {
  constructor(cause) {
    super(cause?.message ?? 'write failed');
    this.cause = cause;
  }
}

async function guarded(fn) {
  try {
    return await fn();
  } catch (err) {
    throw new WriteError(err);
  }
}

const moduleNote = (settings) => (isModuleEnabled(settings, 'birthdays') ? '' : N1);
const isConfigured = (s) => Boolean(s.birthdayChannelId || s.birthdayRoleId);

// ── channel ───────────────────────────────────────────────────────────────────

async function handleChannel(interaction, settings) {
  const guildId = interaction.guild.id;
  const channel = interaction.options.getChannel('channel');

  if (!channel) {
    const next = await guarded(() => updateSettings(guildId, { birthdayChannelId: null }));
    await ensureTickJob(guildId, next, { soon: true }); // may cancel the job when the role is unset too
    return send(interaction, successEmbed('Birthday Announcements Off',
      `Birthdays will no longer be announced. Saved birthdays are kept.${moduleNote(next)}`));
  }

  const u = checkChannelUsable(channel, { needs: ['ViewChannel', 'SendMessages'] });
  if (!u.ok) {
    return send(interaction, errorEmbed('Cannot Use Channel', u.missing.length
      ? `I can't post in ${channel}: missing **${permissionNames(u.missing).join(', ')}**.`
      : `I can't use ${channel}: ${u.reason}.`));
  }
  const next = await guarded(() => updateSettings(guildId, { birthdayChannelId: channel.id }));
  await ensureTickJob(guildId, next, { soon: true });
  return send(interaction, successEmbed('Birthday Channel Set',
    `Birthdays will be announced in ${channel} at **${formatHour(next.birthdayHour)}** (${resolveTimeZone(next)}).${moduleNote(next)}`));
}

// ── role ──────────────────────────────────────────────────────────────────────

async function handleRole(interaction, settings) {
  const { guild } = interaction;
  const role = interaction.options.getRole('role');
  const oldId = settings.birthdayRoleId ?? null;
  const newId = role?.id ?? null;

  if (role) {
    if (role.id === oldId) return send(interaction, infoEmbed('No Change', `${role} is already the birthday role.`));
    const chk = checkAssignableRole(guild, role, { executor: interaction.member, allowDangerous: false });
    if (!chk.ok) return send(interaction, errorEmbed('Cannot Use Role', `${chk.reason}`));
  }

  const next = await guarded(() => updateSettings(guild.id, { birthdayRoleId: newId }));
  // From now on the tick only removes the current role, so the previous one is taken off today's celebrants here.
  const t = oldId && oldId !== newId ? await transferBirthdayRole(guild, oldId, newId) : null;
  await ensureTickJob(guild.id, next, { soon: true });

  let desc = role
    ? `Members get ${role} on their birthday from **${formatHour(next.birthdayHour)}** until the end of that day (${resolveTimeZone(next)}).`
    : 'No role will be given on birthdays.';
  if (t?.removed > 0) desc += `\nI removed the previous birthday role from ${t.removed} member(s) who had it today.`;
  if (t?.failed > 0) desc += `\n⚠️ I couldn't remove the previous birthday role from ${t.failed} member(s). Please remove it manually.`;
  return send(interaction, successEmbed(role ? 'Birthday Role Set' : 'Birthday Role Off', desc + moduleNote(next)));
}

// ── hour ──────────────────────────────────────────────────────────────────────

async function handleHour(interaction) {
  const guildId = interaction.guild.id;
  const hour = interaction.options.getInteger('hour', true);
  const next = await guarded(() => updateSettings(guildId, { birthdayHour: hour }));
  await ensureTickJob(guildId, next, { soon: true }); // only matters when a channel or role is set

  const tz = resolveTimeZone(next);
  let desc = `Birthdays will be announced at **${formatHour(hour)}** (${tz}).`;
  if (zonedParts(new Date(), tz).hour >= hour && isConfigured(next)) {
    desc += "\nToday's birthdays that haven't been celebrated yet will be celebrated in a moment.";
  }
  return send(interaction, successEmbed('Announcement Hour Set', desc + moduleNote(next)));
}

// ── message / message-age ─────────────────────────────────────────────────────

async function handleMessage(interaction, settings, field) {
  const { guild } = interaction;
  const withAge = field === 'birthdayMessageAge';
  const text = (interaction.options.getString('text') ?? '').trim();

  if (!text) {
    const next = await guarded(() => updateSettings(guild.id, { [field]: null }));
    const def = withAge ? DEFAULT_BIRTHDAY_MESSAGE_AGE : DEFAULT_BIRTHDAY_MESSAGE;
    return send(interaction, successEmbed('Birthday Message Reset', `Back to the default: \`${def}\`${moduleNote(next)}`));
  }

  const d = renderPlaceholdersDetailed(text, {
    member: interaction.member, guild, channel: interaction.channel, vars: { age: withAge ? '25' : '' },
  }, { maxLength: 2000 });
  const next = await guarded(() => updateSettings(guild.id, { [field]: text }));

  // Warnings never block saving.
  let desc = `Preview:\n${d.text}`;
  if (d.unknownTokens.length) {
    desc += `\n⚠️ Unknown variables (sent as typed): ${d.unknownTokens.slice(0, 10).map((t) => `\`${t}\``).join(', ')}`;
  }
  if (!withAge && /\{age\}/i.test(text)) {
    desc += '\n⚠️ `{age}` is always empty in this message: it is used when the birth year is unknown.';
  }
  if (withAge && !/\{age\}/i.test(text)) desc += '\nTip: add `{age}` to show the new age.';
  return send(interaction, successEmbed(withAge ? 'Birthday Message (With Age) Updated' : 'Birthday Message Updated',
    clampText(desc + moduleNote(next), 4096)));
}

// ── require-year ──────────────────────────────────────────────────────────────

async function handleRequireYear(interaction) {
  const guildId = interaction.guild.id;
  const enabled = interaction.options.getBoolean('enabled', true);
  const next = await guarded(() => updateSettings(guildId, { birthdayRequireYear: enabled }));
  // Existing rows are never changed.
  if (!enabled) {
    return send(interaction, successEmbed('Birth Year Optional', `Members can use **MM-DD** or **YYYY-MM-DD**.${moduleNote(next)}`));
  }
  const { withoutYear } = await countBirthdays(guildId).catch(() => ({ withoutYear: 0 }));
  let desc = 'Members must now use **YYYY-MM-DD**.';
  if (withoutYear > 0) desc += ` ${withoutYear} saved birthday(s) without a year are kept and use the message without age.`;
  return send(interaction, successEmbed('Birth Year Required', desc + moduleNote(next)));
}

// ── set-user / unset-user ─────────────────────────────────────────────────────

async function handleSetUser(interaction, settings) {
  const { guild } = interaction;
  const member = interaction.options.getMember('member');
  if (!member) return send(interaction, E1());
  if (member.user.bot) return send(interaction, E2());

  const now = new Date();
  // An admin may enter MM-DD even when members must give a year: admins often know the date but not the year.
  const r = parseBirthdayInput(interaction.options.getString('date', true), { requireYear: false, now, tz: resolveTimeZone(settings) });
  if (!r.ok) return send(interaction, errorEmbed('Invalid Date', r.message));

  const res = await guarded(() => saveBirthday({ guild, userId: member.id, date: r, settings, now }));
  let desc = `${member}'s birthday is now **${formatBirthday(r)}**.`; // no year: the reply is public
  if (res.late) desc += "\nIt is today and today's celebration time has passed, so it counts from next year.";
  else if (res.alreadyCelebrated) desc += '\nThey were already celebrated this year, so the new date counts from next year.';
  return send(interaction, successEmbed('Birthday Saved', desc + moduleNote(settings)));
}

async function handleUnsetUser(interaction, settings) {
  // The user need not be a member any more, so a departed member's data can be removed too.
  const user = interaction.options.getUser('member', true);
  const res = await guarded(() => removeBirthday({ guild: interaction.guild, userId: user.id, settings }));
  return send(interaction, res.removed
    ? successEmbed('Birthday Removed', `${user}'s birthday was removed.`)
    : infoEmbed('Nothing to Remove', `${user} has no saved birthday.`));
}

// ── show ──────────────────────────────────────────────────────────────────────

function channelField(guild, settings) {
  if (!settings.birthdayChannelId) return 'Not set (no announcements)';
  const ch = guild.channels.cache.get(settings.birthdayChannelId);
  if (!ch) return '⚠️ Deleted channel — set a new one';
  const u = checkChannelUsable(ch, { needs: ['ViewChannel', 'SendMessages'] });
  if (u.ok) return `<#${ch.id}>`;
  const names = u.missing.length ? permissionNames(u.missing).join(', ') : '';
  return `<#${ch.id}> ⚠️ missing ${names || u.reason}`;
}

function roleField(guild, settings) {
  if (!settings.birthdayRoleId) return 'Not set';
  const role = guild.roles.cache.get(settings.birthdayRoleId);
  if (!role) return '⚠️ Deleted role — set a new one';
  const chk = checkAssignableRole(guild, role, { allowDangerous: false });
  return chk.ok ? `<@&${role.id}>` : `<@&${role.id}> ⚠️ ${chk.reason}`;
}

async function handleShow(interaction, settings) {
  const { guild } = interaction;
  const configured = isConfigured(settings);
  if (configured) await ensureTickJob(guild.id, settings); // self-heal (never throws)
  const [job, counts] = await Promise.all([getJob(tickKey(guild.id)), countBirthdays(guild.id)]);

  const tz = resolveTimeZone(settings);
  let next;
  if (!configured) next = 'Off (no channel or role set)';
  else if (job && (job.status === 'PENDING' || job.status === 'RUNNING')) next = discordTimestamp(job.runAt, 'R');
  else next = '⚠️ Not scheduled — run this again in a minute';

  const messageField = (value, def) => `${codeBlock(value ?? def)}${value == null ? '\n(default)' : ''}`;
  const embed = guildEmbed(settings, '🎂 Birthday Settings',
    'Variables: `{user}` `{user.name}` `{user.display_name}` `{server}` `{server.member_count}` `{age}` and the other MEE6 placeholders.');
  const field = (name, value, inline = false) => embed.addFields({ name, value: clampText(value, 1024), inline });
  field('Module', isModuleEnabled(settings, 'birthdays') ? '✅ On' : '❌ Off — `/module enable birthdays`', true);
  field('Channel', channelField(guild, settings), true);
  field('Role', roleField(guild, settings), true);
  field('Announcement hour', `**${formatHour(settings.birthdayHour)}** (${tz})`, true);
  field('Birth year', settings.birthdayRequireYear ? 'Required' : 'Optional', true);
  field('Saved birthdays', `${formatNumber(counts.total)}${counts.withoutYear > 0 ? ` (${formatNumber(counts.withoutYear)} without a year)` : ''}`, true);
  field('Next check', next, true);
  field('Message (age unknown)', messageField(settings.birthdayMessage, DEFAULT_BIRTHDAY_MESSAGE));
  field('Message (age known)', messageField(settings.birthdayMessageAge, DEFAULT_BIRTHDAY_MESSAGE_AGE));
  return send(interaction, embed);
}

// ── test ──────────────────────────────────────────────────────────────────────

async function handleTest(interaction, settings) {
  const { guild } = interaction;
  const tz = resolveTimeZone(settings);
  const channel = guild.channels.cache.get(settings.birthdayChannelId) ?? interaction.channel;
  const line = (age) => renderBirthdayLine({
    template: pickTemplate(settings, age), member: interaction.member, guild, channel, age,
  });
  let desc = `**Without a birth year**\n${line(null)}`;
  desc += `\n\n**With a birth year** (example age: 25)\n${line(25)}`;
  desc += `\n\n${settings.birthdayChannelId
    ? `Posted in <#${settings.birthdayChannelId}> at **${formatHour(settings.birthdayHour)}** (${tz}).`
    : 'Announcements are off: no channel is set.'}`;
  return send(interaction, guildEmbed(settings, '🎂 Birthday Preview', clampText(desc, 4096)));
}

const HANDLERS = Object.freeze({
  channel: handleChannel,
  role: handleRole,
  hour: handleHour,
  message: (i, s) => handleMessage(i, s, 'birthdayMessage'),
  'message-age': (i, s) => handleMessage(i, s, 'birthdayMessageAge'),
  'require-year': handleRequireYear,
  'set-user': handleSetUser,
  'unset-user': handleUnsetUser,
  show: handleShow,
  test: handleTest,
});

// ── Entry point ───────────────────────────────────────────────────────────────

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  const handler = HANDLERS[sub];
  if (!handler) return;
  // The acknowledgement comes first (00-overview rule 21); `test` is the one private reply.
  if (!(await safeDefer(interaction, { ephemeral: sub === 'test' }))) return;
  try {
    const settings = await getSettingsSafe(interaction.guild.id);
    if (!settings) return await send(interaction, U1());
    await handler(interaction, settings);
  } catch (err) {
    if (err instanceof WriteError) {
      logger.error(`birthday-config ${sub} failed`, err.cause ?? err);
      await send(interaction, G2());
    } else {
      logger.error(`birthday-config ${sub} failed`, err);
      await send(interaction, G1());
    }
  }
}
