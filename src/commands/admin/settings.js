// /settings: general server settings (MEE6 Settings): timezone, default embed colour, custom-command
// prefix, the /urban age-restriction switch and a read-only overview. Every read and write goes
// through settingsService (00-overview rule 8); nothing here touches Prisma or Redis directly.
import { SlashCommandBuilder, PermissionFlagsBits, escapeMarkdown } from 'discord.js';
import { PERMISSION_LEVELS, COLORS } from '../../config/constants.js';
import { MODULES, MODULE_NAMES, isModuleEnabled } from '../../config/modules.js';
import { getSettingsSafe, peekSettings, updateSettings } from '../../services/settingsService.js';
import { successEmbed, errorEmbed, infoEmbed, guildEmbed, applyGuildColor } from '../../utils/embedBuilder.js';
import { parseColor, formatColor } from '../../utils/customEmbed.js';
import { isValidTimeZone, zonedParts, formatTimeOfDay } from '../../utils/time.js';
import { clampText, joinWithLimit } from '../../utils/text.js';
import { safeDefer, safeReply, replyError, respondAutocomplete } from '../../utils/interactionHelpers.js';
import logger from '../../utils/logger.js';

export const module = null; // core command: never module-gated (00-overview §7 row 38)
export const permissionLevel = PERMISSION_LEVELS.ADMIN;

export const data = new SlashCommandBuilder()
  .setName('settings')
  .setDescription('Server settings: timezone, embed colour, custom-command prefix, /urban (MEE6 Settings).')
  .setDefaultMemberPermissions(PermissionFlagsBits.ManageGuild)
  .addSubcommand((s) => s.setName('show')
    .setDescription('Show the general settings, enabled modules and moderator roles.'))
  .addSubcommand((s) => s.setName('timezone')
    .setDescription('Set the server timezone used by reminders, birthdays and other schedules.')
    .addStringOption((o) => o.setName('zone')
      .setDescription('Region/City, e.g. Europe/London or America/New_York. Type an offset like +5:30 to search.')
      .setRequired(true).setMaxLength(64).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('color')
    .setDescription('Set the colour of info embeds, or reset it to the default.')
    .addStringOption((o) => o.setName('value')
      .setDescription('Hex (#5865F2 or #fff), a colour name (red, blurple…) or reset.')
      .setRequired(true).setMaxLength(16).setAutocomplete(true)))
  .addSubcommand((s) => s.setName('prefix')
    .setDescription('Set the prefix for custom commands (MEE6 !command). Default: !')
    .addStringOption((o) => o.setName('prefix')
      .setDescription('1–5 characters, no spaces, e.g. ! ? . $ or +. Type off to disable.')
      .setRequired(true).setMinLength(1).setMaxLength(5)))
  .addSubcommand((s) => s.setName('urban')
    .setDescription('Choose where /urban (Urban Dictionary) may be used.')
    .addBooleanOption((o) => o.setName('nsfw-only')
      .setDescription('True: age-restricted channels only (default). False: every channel.')
      .setRequired(true)));

// ── Timezones ────────────────────────────────────────────────────────────────

// ICU still lists legacy IANA names. Map them to the current tzdata names so
// users find "Kolkata"/"Kyiv". A no-op on runtimes that already list the new names.
export const LEGACY_TZ_NAMES = Object.freeze({
  'Africa/Asmera': 'Africa/Asmara', 'America/Buenos_Aires': 'America/Argentina/Buenos_Aires',
  'America/Catamarca': 'America/Argentina/Catamarca', 'America/Cordoba': 'America/Argentina/Cordoba',
  'America/Jujuy': 'America/Argentina/Jujuy', 'America/Mendoza': 'America/Argentina/Mendoza',
  'America/Coral_Harbour': 'America/Atikokan', 'America/Godthab': 'America/Nuuk',
  'America/Indianapolis': 'America/Indiana/Indianapolis', 'America/Louisville': 'America/Kentucky/Louisville',
  'Asia/Calcutta': 'Asia/Kolkata', 'Asia/Katmandu': 'Asia/Kathmandu', 'Asia/Rangoon': 'Asia/Yangon',
  'Asia/Saigon': 'Asia/Ho_Chi_Minh', 'Atlantic/Faeroe': 'Atlantic/Faroe', 'Europe/Kiev': 'Europe/Kyiv',
  'Pacific/Enderbury': 'Pacific/Kanton', 'Pacific/Ponape': 'Pacific/Pohnpei', 'Pacific/Truk': 'Pacific/Chuuk',
});

// Lower-cased names that all mean UTC. They are accepted and stored as 'UTC'.
const UTC_ALIASES = new Set(['utc', 'gmt', 'uct', 'zulu', 'universal', 'greenwich', 'etc/utc', 'etc/gmt',
  'etc/uct', 'etc/zulu', 'etc/universal', 'etc/greenwich', 'gmt0', 'gmt+0', 'gmt-0', 'etc/gmt0', 'etc/gmt+0', 'etc/gmt-0']);

// Shown when the zone box is empty (after the current zone), in this order.
export const POPULAR_TIMEZONES = Object.freeze(['UTC', 'America/New_York', 'America/Chicago', 'America/Denver',
  'America/Los_Angeles', 'America/Toronto', 'America/Mexico_City', 'America/Sao_Paulo', 'Europe/London',
  'Europe/Paris', 'Europe/Berlin', 'Europe/Madrid', 'Europe/Istanbul', 'Europe/Moscow', 'Africa/Cairo',
  'Africa/Johannesburg', 'Asia/Dubai', 'Asia/Kolkata', 'Asia/Bangkok', 'Asia/Singapore', 'Asia/Shanghai',
  'Asia/Tokyo', 'Asia/Seoul', 'Australia/Sydney', 'Pacific/Auckland']);

let zoneList = null; // lazily built; never at import
const offsetFormatters = new Map(); // zone → Intl.DateTimeFormat (≤ ~430 entries)

/**
 * Every timezone the autocomplete offers: 'UTC' first, then the ICU zones (legacy names mapped to
 * the current tzdata names), sorted. Built once on first use.
 * @returns {readonly string[]}
 */
export function listTimeZones() {
  if (zoneList) return zoneList;
  const out = new Set(['UTC']);
  for (const z of Intl.supportedValuesOf('timeZone')) {
    const modern = LEGACY_TZ_NAMES[z];
    out.add(modern && isValidTimeZone(modern) ? modern : z);
  }
  zoneList = Object.freeze([...out].sort((a, b) => (a === 'UTC' ? -1 : b === 'UTC' ? 1 : a.localeCompare(b))));
  return zoneList;
}

/**
 * Validates and canonicalises a timezone typed by an admin.
 * @param {unknown} input
 * @returns {{ ok: true, zone: string } | { ok: false, code: 'EMPTY'|'UNKNOWN'|'OFFSET'|'ABBREVIATION' }}
 */
export function normalizeTimeZone(input) {
  const s = String(input ?? '').trim(); // Intl throws on untrimmed names
  if (!s) return { ok: false, code: 'EMPTY' };
  if (s.length > 64) return { ok: false, code: 'UNKNOWN' };
  if (UTC_ALIASES.has(s.toLowerCase())) return { ok: true, zone: 'UTC' };
  if (/^[+\-−\d]/.test(s) || /^(?:utc|gmt)\s*[+\-−]\s*\d/i.test(s)) return { ok: false, code: 'OFFSET' };
  if (!isValidTimeZone(s)) return { ok: false, code: 'UNKNOWN' };
  if (!s.includes('/')) return { ok: false, code: 'ABBREVIATION' }; // EST → America/Panama, CET, Japan …
  const lower = s.toLowerCase();
  const hit = listTimeZones().find((z) => z.toLowerCase() === lower);
  if (hit) return { ok: true, zone: hit };
  const canonical = new Intl.DateTimeFormat('en-US', { timeZone: s }).resolvedOptions().timeZone;
  if (UTC_ALIASES.has(canonical.toLowerCase())) return { ok: true, zone: 'UTC' };
  const modern = LEGACY_TZ_NAMES[canonical];
  return { ok: true, zone: modern && isValidTimeZone(modern) ? modern : canonical };
}

/**
 * 'UTC+05:30'-style label for a zone at an instant (DST aware). Never throws.
 * @param {string} zone
 * @param {Date} [date]
 * @returns {string}
 */
export function utcOffsetLabel(zone, date = new Date()) {
  try {
    let fmt = offsetFormatters.get(zone);
    if (!fmt) {
      fmt = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' });
      offsetFormatters.set(zone, fmt);
    }
    const raw = (fmt.formatToParts(date).find((p) => p.type === 'timeZoneName')?.value ?? 'GMT').replace('−', '-');
    const m = /^GMT([+-]\d{2}:\d{2})$/.exec(raw);
    return m ? `UTC${m[1]}` : 'UTC+00:00'; // bare "GMT" = +00:00
  } catch {
    return 'UTC';
  }
}

/**
 * Autocomplete choices for the zone option: by name or city, or by offset ('+5:30', 'utc-3').
 * @param {unknown} query
 * @param {{ current?: string|null, now?: Date, limit?: number }} [opts]
 * @returns {{ name: string, value: string }[]}
 */
export function searchTimeZones(query, { current = null, now = new Date(), limit = 25 } = {}) {
  const list = listTimeZones();
  const known = new Set(list);
  const q = String(query ?? '').trim().toLowerCase().replace(/\s+/g, '_');
  const off = /^(?:utc|gmt)?_?([+\-−])_?(\d{1,2})(?::?(\d{2}))?$/.exec(q);
  let zones;
  if (!q) {
    const cur = current && known.has(current) ? [current] : [];
    zones = [...cur, ...POPULAR_TIMEZONES.filter((z) => known.has(z) && z !== current)];
  } else if (off) {
    const hh = Number(off[2]);
    if (hh > 14) return [];
    const tail = `${String(hh).padStart(2, '0')}${off[3] ? `:${off[3]}` : ''}`;
    const prefix = `UTC${off[1] === '+' ? '+' : '-'}${tail}`;
    const zeroPrefix = `UTC+${tail}`; // "-0" also finds UTC+00:00 zones
    zones = list
      .map((z) => [z, utcOffsetLabel(z, now)])
      .filter(([, label]) => label.startsWith(prefix) || (hh === 0 && label.startsWith(zeroPrefix)))
      .sort((a, b) => a[1].localeCompare(b[1]) || a[0].localeCompare(b[0]))
      .map(([z]) => z);
  } else {
    const scored = [];
    for (const z of list) {
      const zl = z.toLowerCase();
      const city = zl.slice(zl.lastIndexOf('/') + 1);
      let score = -1;
      if (zl === q || city === q) score = 0;
      else if (city.startsWith(q)) score = 1;
      else if (zl.startsWith(q)) score = 2;
      else if (zl.includes(q)) score = 3;
      if (score >= 0) scored.push([score, z]);
    }
    scored.sort((a, b) => a[0] - b[0] || a[1].localeCompare(b[1]));
    zones = scored.map(([, z]) => z);
  }
  return zones.slice(0, limit).map((z) => ({
    name: `${z} (${utcOffsetLabel(z, now)})${z === current ? ' · current' : ''}`,
    value: z,
  }));
}

const WEEKDAY_LONG = Object.freeze(['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday']);
const pad2 = (n) => String(n).padStart(2, '0');

/**
 * Local time in a zone, for replies. An invalid zone falls back to UTC (`valid` is then false).
 * @param {string} zone
 * @param {Date} [now]
 * @returns {{ valid: boolean, zone: string, time: string, date: string, weekday: string, offset: string }}
 */
export function describeLocalTime(zone, now = new Date()) {
  const tz = isValidTimeZone(zone) ? zone : 'UTC';
  const p = zonedParts(now, tz);
  return {
    valid: tz === zone,
    zone: tz,
    time: formatTimeOfDay({ hour: p.hour, minute: p.minute }),
    date: `${p.year}-${pad2(p.month)}-${pad2(p.day)}`,
    weekday: WEEKDAY_LONG[p.weekday],
    offset: utcOffsetLabel(tz, now),
  };
}

// ── Colour ───────────────────────────────────────────────────────────────────

export const COLOR_NAMES = Object.freeze(['blurple', 'red', 'green', 'blue', 'yellow', 'orange', 'purple', 'pink', 'white', 'black', 'grey']);
export const COLOR_RESET_WORDS = Object.freeze(['reset', 'default', 'none', 'clear']);

/**
 * Parses the colour option. `color: null` means "reset to the default".
 * @param {unknown} input
 * @returns {{ ok: true, color: number|null, adjusted: boolean } | { ok: false }}
 */
export function parseColorInput(input) {
  const s = String(input ?? '').trim().toLowerCase();
  if (!s) return { ok: false };
  if (COLOR_RESET_WORDS.includes(s)) return { ok: true, color: null, adjusted: false };
  const c = parseColor(s);
  if (!Number.isInteger(c) || c < 0 || c > 0xFFFFFF) return { ok: false };
  if (c === 0) return { ok: true, color: 0x010101, adjusted: true }; // Discord renders 0 as "no colour"
  return { ok: true, color: c, adjusted: false };
}

/**
 * Autocomplete choices for the colour option (at most 25; every value ≤ 16 characters).
 * @param {unknown} query
 * @param {{ current?: number|null }} [opts]
 * @returns {{ name: string, value: string }[]}
 */
export function suggestColors(query, { current = null } = {}) {
  const q = String(query ?? '').trim().toLowerCase();
  const out = [];
  if (!q && Number.isInteger(current)) {
    out.push({ name: `Current: ${formatColor(current)}`, value: formatColor(current) });
  }
  const parsed = q ? parseColorInput(q) : null;
  if (parsed?.ok && parsed.color !== null && !COLOR_NAMES.includes(q)) {
    out.push({ name: `${formatColor(parsed.color)} (custom)`, value: String(query).trim().slice(0, 16) });
  }
  if (!q || COLOR_RESET_WORDS.some((w) => w.startsWith(q))) {
    out.push({ name: `reset (default colour ${formatColor(COLORS.INFO)})`, value: 'reset' });
  }
  for (const n of COLOR_NAMES) {
    if (q && !n.includes(q)) continue;
    const c = parseColor(n);
    if (c !== null) out.push({ name: `${n} (${formatColor(c)})`, value: n });
  }
  return out.slice(0, 25);
}

// ── Prefix ───────────────────────────────────────────────────────────────────

export const PREFIX_MAX = 5;
const PREFIX_FORBIDDEN = /[`<>@#:\\/]/u; // commands (/), mentions (<@ <# <@&), emoji (:), markdown escapes/code

/**
 * Validates the custom-command prefix. `off` (any case) is accepted and stored as the empty
 * string, which disables prefix invocation of custom commands (/tag use keeps working).
 * @param {unknown} input
 * @returns {{ ok: true, prefix: string, off?: true } | { ok: false, code: 'EMPTY'|'SPACES'|'TOO_LONG'|'INVALID_CHARS' }}
 */
export function validatePrefix(input) {
  const s = String(input ?? '').trim();
  if (!s) return { ok: false, code: 'EMPTY' };
  if (s.toLowerCase() === 'off') return { ok: true, prefix: '', off: true };
  if (/\s/u.test(s)) return { ok: false, code: 'SPACES' };
  if ([...s].length > PREFIX_MAX) return { ok: false, code: 'TOO_LONG' };
  if (/\p{C}/u.test(s) || PREFIX_FORBIDDEN.test(s)) return { ok: false, code: 'INVALID_CHARS' };
  return { ok: true, prefix: s };
}

// ── Overview ─────────────────────────────────────────────────────────────────

/**
 * The `/settings show` embed. Pure: `guild` needs `name` and `roles.cache` (Map-like with `.has`).
 * @param {object} settings a GuildSettings row
 * @param {{ name?: string, roles: { cache: { has(id: string): boolean } } }} guild
 * @param {{ now?: Date }} [opts]
 * @returns {import('discord.js').EmbedBuilder}
 */
export function buildOverviewEmbed(settings, guild, { now = new Date() } = {}) {
  // Timezone
  const lt = describeLocalTime(settings.timezone, now);
  const tzValue = lt.valid
    ? `\`${settings.timezone}\`\n${lt.offset} · local time **${lt.time}** (${lt.weekday})`
    : `\`${clampText(String(settings.timezone ?? '').replace(/`/g, ''), 64)}\` ⚠️ not a valid timezone, so UTC is used. Fix it with \`/settings timezone\`.`;

  // Colour
  const colorValue = Number.isInteger(settings.embedColor)
    ? `\`${formatColor(settings.embedColor)}\``
    : `Default (\`${formatColor(COLORS.INFO)}\`)`;

  // Prefix. '' means prefix invocation is off (/settings prefix off); null/undefined (legacy) shows the default.
  // A hand-edited prefix containing a backtick only breaks the inline-code formatting; acceptable.
  const customCommandsLine = isModuleEnabled(settings, 'customcommands')
    ? 'Custom commands: on'
    : 'Custom commands are off · `/module enable customcommands`';
  const prefixValue = settings.customPrefix === ''
    ? `Prefix: off · \`/tag use\` only\n${customCommandsLine}`
    : `\`${settings.customPrefix ?? '!'}\`\n${customCommandsLine}`;

  // Modules
  const enabled = MODULE_NAMES.filter((k) => isModuleEnabled(settings, k));
  const head = `**${enabled.length}** of **${MODULE_NAMES.length}** enabled · \`/module list\``;
  const modulesValue = enabled.length
    ? `${head}\n${joinWithLimit(enabled.map((k) => `${MODULES[k].emoji} ${MODULES[k].label}`), { max: 1024 - head.length - 1 })}`
    : head;

  // Urban Dictionary gate. Fail closed, like /urban itself: only an explicit false opens it up.
  const urbanValue = settings.urbanNsfwOnly === false
    ? 'Urban Dictionary: everywhere · `/settings urban`'
    : 'Urban Dictionary: age-restricted channels only · `/settings urban`';

  // Moderator roles (read-only; edited in /modconfig)
  const ids = [...new Set(settings.modRoleIds ?? [])];
  const live = ids.filter((id) => guild.roles.cache.has(id));
  const missing = ids.length - live.length;
  const tail = 'Edit with `/modconfig`. Admins: Administrator or Manage Server. Moderators: these roles, or Kick/Ban/Timeout Members.';
  let roles = live.length
    ? joinWithLimit(live.map((id) => `<@&${id}>`), { max: 1024 - tail.length - 40 })
    : 'None set';
  if (missing > 0) roles += ` (+${missing} deleted role${missing === 1 ? '' : 's'})`;

  const embed = guildEmbed(
    settings,
    '⚙️ Server Settings',
    `General settings for **${escapeMarkdown(clampText(guild.name ?? 'this server', 100))}**.`,
    'Change with /settings timezone · color · prefix · urban',
  );
  embed.addFields(
    { name: '🌍 Timezone', value: clampText(tzValue, 1024), inline: true },
    { name: '🎨 Embed colour', value: colorValue, inline: true },
    { name: '❗ Custom-command prefix', value: clampText(prefixValue, 1024), inline: true },
    { name: '🧩 Modules', value: clampText(modulesValue, 1024) },
    { name: '🔎 Search', value: urbanValue },
    { name: '🛡️ Moderator roles', value: clampText(`${roles}\n${tail}`, 1024) },
  );
  return embed; // 6 fields (≤ 25), total far below 6000
}

// ── Shared reply plumbing ────────────────────────────────────────────────────

const reply = (embeds) => ({ embeds, allowedMentions: { parse: [] } });

// Echoes user input inside an error text: no code-span breakers, bounded.
const shown = (raw) => clampText(String(raw ?? '').replace(/[`\\]/g, '').trim(), 64) || '(empty)';

const TZ_ERRORS = {
  EMPTY: ['Missing Timezone', () => 'Pick a timezone from the list, e.g. `Europe/London`.'],
  UNKNOWN: ['Unknown Timezone', (raw) => '`' + shown(raw) + '` is not a timezone I know. Pick one from the list, e.g. `Europe/London`, `America/New_York` or `Asia/Tokyo`.'],
  OFFSET: ['Pick a City', () => 'Fixed offsets like `+05:30` ignore daylight saving time. Type the offset in the **zone** box to see the cities that use it, then pick one.'],
  ABBREVIATION: ['Ambiguous Timezone', () => 'Abbreviations and country names such as `EST`, `CET` or `Japan` are ambiguous. Pick a Region/City zone such as `America/New_York`, `Europe/Paris` or `Asia/Tokyo`.'],
};

const PREFIX_ERRORS = {
  EMPTY: "The prefix can't be empty.",
  SPACES: "The prefix can't contain spaces.",
  TOO_LONG: 'The prefix can be at most 5 characters.',
  INVALID_CHARS: "The prefix can't contain `/`, `@`, `#`, `:`, `<`, `>`, backslashes, backticks or invisible characters, because Discord uses them for commands, mentions and emoji.",
};

/**
 * Writes one setting through settingsService. Returns the saved row, or null after answering
 * "Could Not Save" (the caller just returns).
 */
async function saveSetting(interaction, sub, field, value, previous) {
  const guildId = interaction.guildId;
  let row;
  try {
    row = await updateSettings(guildId, { [field]: value });
  } catch (err) {
    logger.error(`[settings] ${sub} save failed`, err, { guildId });
    await safeReply(interaction, reply([errorEmbed('Could Not Save', 'The setting could not be saved. Please try again in a moment.')]));
    return null;
  }
  const oldValue = previous?.[field] ?? null;
  logger.info(`[settings] ${guildId}: ${field} ${JSON.stringify(oldValue)} → ${JSON.stringify(value)} by ${interaction.user?.id}`);
  return row;
}

const noChange = (interaction, previous, text) =>
  safeReply(interaction, reply([applyGuildColor(infoEmbed('No Change', text), previous)]));

// ── Subcommands ──────────────────────────────────────────────────────────────

async function runShow(interaction) {
  if (!(await safeDefer(interaction))) return;
  const settings = await getSettingsSafe(interaction.guildId);
  if (!settings) {
    return safeReply(interaction, reply([errorEmbed('Settings Unavailable', 'Server settings could not be loaded. Please try again in a moment.')]));
  }
  return safeReply(interaction, reply([buildOverviewEmbed(settings, interaction.guild)]));
}

async function runTimezone(interaction) {
  const raw = interaction.options.getString('zone', true);
  const r = normalizeTimeZone(raw);
  if (!r.ok) {
    const [title, text] = TZ_ERRORS[r.code];
    return replyError(interaction, title, text(raw));
  }
  if (!(await safeDefer(interaction))) return;
  const previous = await getSettingsSafe(interaction.guildId);
  if (previous?.timezone === r.zone) {
    return noChange(interaction, previous, `The server timezone is already **${r.zone}** (${utcOffsetLabel(r.zone)}).`);
  }
  const row = await saveSetting(interaction, 'timezone', 'timezone', r.zone, previous);
  if (!row) return;

  const lt = describeLocalTime(r.zone);
  const lines = [
    `The server timezone is now **${r.zone}** (${lt.offset}).`,
    `Local time there: **${lt.time}** on ${lt.weekday}, ${lt.date}.`,
  ];
  if (previous?.timezone && previous.timezone !== r.zone) lines.push(`Previously: \`${shown(previous.timezone)}\`.`);
  lines.push('', 'Reminders, birthdays and other schedules use this timezone from their next run.');
  return safeReply(interaction, reply([successEmbed('Timezone Updated', lines.join('\n'))]));
}

async function runColor(interaction) {
  const raw = interaction.options.getString('value', true);
  const r = parseColorInput(raw);
  if (!r.ok) {
    return replyError(interaction, 'Invalid Colour',
      `\`${shown(raw)}\` is not a colour. Use a hex code (\`#5865F2\` or \`#fff\`), a name (red, green, blue, blurple, yellow, orange, purple, pink, white, black, grey) or \`reset\`.`);
  }
  if (!(await safeDefer(interaction))) return;
  const previous = await getSettingsSafe(interaction.guildId);
  if (previous && (previous.embedColor ?? null) === r.color) {
    return noChange(interaction, previous, r.color === null
      ? `The embed colour is already the default (**${formatColor(COLORS.INFO)}**).`
      : `The embed colour is already **${formatColor(r.color)}**.`);
  }
  const row = await saveSetting(interaction, 'color', 'embedColor', r.color, previous);
  if (!row) return;

  const first = r.color === null
    ? successEmbed('Embed Colour Reset', `Info embeds use the default colour **${formatColor(COLORS.INFO)}** again.`)
    : successEmbed('Embed Colour Updated', [
      `Info embeds now use **${formatColor(r.color)}**.`,
      ...(r.adjusted ? ['Pure black (#000000) shows as no colour in Discord, so **#010101** was saved instead.'] : []),
      'Success, error and warning embeds keep their fixed colours.',
    ].join('\n'));
  const preview = guildEmbed(row, '🎨 Preview', 'This is how info embeds from this bot look now.'); // shows the new colour
  return safeReply(interaction, reply([first, preview]));
}

async function runPrefix(interaction) {
  const raw = interaction.options.getString('prefix', true);
  const r = validatePrefix(raw);
  if (!r.ok) return replyError(interaction, 'Invalid Prefix', PREFIX_ERRORS[r.code]);
  if (!(await safeDefer(interaction))) return;
  const previous = await getSettingsSafe(interaction.guildId);
  if (previous?.customPrefix === r.prefix) {
    return noChange(interaction, previous, r.prefix === ''
      ? 'Prefix commands are already off. `/tag use` still works.'
      : `The custom-command prefix is already \`${r.prefix}\`.`);
  }
  const row = await saveSetting(interaction, 'prefix', 'customPrefix', r.prefix, previous);
  if (!row) return;

  if (r.prefix === '') {
    return safeReply(interaction, reply([successEmbed('Prefix Updated', 'Prefix commands are off. /tag use still works.')]));
  }
  let text = `Custom commands now answer to \`${r.prefix}name\`, e.g. \`${r.prefix}rules\`.\nSlash commands are not affected.`;
  if (!isModuleEnabled(row, 'customcommands')) {
    text += '\n\n⚠️ The **Custom Commands** module is off, so the prefix does nothing yet. Enable it with `/module enable customcommands`.';
  }
  return safeReply(interaction, reply([successEmbed('Prefix Updated', text)]));
}

async function runUrban(interaction) {
  const nsfwOnly = interaction.options.getBoolean('nsfw-only', true);
  if (!(await safeDefer(interaction))) return;
  const previous = await getSettingsSafe(interaction.guildId);
  if (previous && typeof previous.urbanNsfwOnly === 'boolean' && previous.urbanNsfwOnly === nsfwOnly) {
    return noChange(interaction, previous, nsfwOnly
      ? '/urban already only works in age-restricted channels.'
      : '/urban already works in every channel.');
  }
  const row = await saveSetting(interaction, 'urban', 'urbanNsfwOnly', nsfwOnly, previous);
  if (!row) return;

  let text = nsfwOnly
    ? '/urban now only works in age-restricted channels.'
    : '/urban now works in every channel.';
  if (!isModuleEnabled(row, 'search')) {
    text += '\n\n⚠️ The **Search Anything** module is off, so /urban is unavailable. Enable it with `/module enable search`.';
  }
  return safeReply(interaction, reply([successEmbed('Urban Dictionary', text)]));
}

export async function execute(interaction) {
  const sub = interaction.options.getSubcommand();
  if (sub === 'show') return runShow(interaction);
  if (sub === 'timezone') return runTimezone(interaction);
  if (sub === 'color') return runColor(interaction);
  if (sub === 'prefix') return runPrefix(interaction);
  if (sub === 'urban') return runUrban(interaction);
}

export async function autocomplete(interaction) {
  let choices = [];
  try {
    const sub = interaction.options.getSubcommand(false);
    const focused = interaction.options.getFocused(true); // { name, value }
    const current = peekSettings(interaction.guildId); // sync memo only; may be null: no I/O on keystrokes
    if (sub === 'timezone' && focused.name === 'zone') {
      choices = searchTimeZones(focused.value, { current: current?.timezone ?? null });
    } else if (sub === 'color' && focused.name === 'value') {
      choices = suggestColors(focused.value, { current: current?.embedColor ?? null });
    }
  } catch (err) {
    logger.warn(`[settings] autocomplete failed: ${err?.message}`);
    choices = [];
  }
  await respondAutocomplete(interaction, choices); // clamps to 25 / 100 chars and catches
}
