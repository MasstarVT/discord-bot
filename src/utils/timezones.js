// ── Timezone names: validation, search and display ─────────────────────────────
// Pure (Intl only, nothing built at import). Shared by /settings timezone, /settings timezone-role,
// /timezone and /timestamp. The dashboard keeps its own copy of the validation rules (fields.js).
import { isValidTimeZone, zonedParts, formatTimeOfDay, zoneFromAbbreviation, zoneFromOffset } from './time.js';

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
 * With `abbreviations`, an abbreviation such as 'cst' puts the zone it names first (parseZoneInput).
 * @param {unknown} query
 * @param {{ current?: string|null, now?: Date, limit?: number, abbreviations?: boolean }} [opts]
 * @returns {{ name: string, value: string }[]}
 */
export function searchTimeZones(query, { current = null, now = new Date(), limit = 25, abbreviations = false } = {}) {
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
  const abbr = abbreviations ? zoneFromAbbreviation(query) : null;
  if (abbr) zones = [abbr, ...zones.filter((z) => z !== abbr)];
  return zones.slice(0, limit).map((z) => ({
    name: `${z} (${utcOffsetLabel(z, now)})${z === abbr && z !== 'UTC' ? ` · ${String(query).trim().toUpperCase()}` : ''}${z === current ? ' · current' : ''}`,
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

// ── Member input (/timezone, /settings timezone-role) ─────────────────────────

// Abbreviations that name several zones, with the zones to suggest instead.
const AMBIGUOUS_ABBREVIATIONS = Object.freeze({
  ist: ['Asia/Kolkata', 'Europe/Dublin', 'Asia/Jerusalem'],
  ast: ['America/Halifax', 'America/Puerto_Rico', 'Asia/Riyadh'],
});

/**
 * A timezone a member typed: a Region/City zone or 'UTC' (normalizeTimeZone), or a common
 * abbreviation such as CST, read as the region it names with daylight saving (ZONE_ABBREVIATIONS).
 * Fixed offsets are refused as in normalizeTimeZone.
 * @param {unknown} input
 * @returns {{ ok: true, zone: string, abbreviation: string|null }
 *   | { ok: false, code: 'EMPTY'|'UNKNOWN'|'OFFSET'|'ABBREVIATION'|'AMBIGUOUS', options?: string[] }}
 */
export function parseZoneInput(input) {
  const s = String(input ?? '').trim();
  const named = zoneFromAbbreviation(s);
  if (named) return { ok: true, zone: named, abbreviation: named === 'UTC' ? null : s.toUpperCase() };
  const lower = s.toLowerCase();
  if (Object.hasOwn(AMBIGUOUS_ABBREVIATIONS, lower)) return { ok: false, code: 'AMBIGUOUS', options: [...AMBIGUOUS_ABBREVIATIONS[lower]] };
  const r = normalizeTimeZone(s);
  return r.ok ? { ...r, abbreviation: null } : r;
}

/**
 * The zone a timezone role's name names, for /settings timezone-role add without a zone:
 * an abbreviation ('CST', 'CST/CDT', '🇺🇸 Eastern (EST)') or a whole-hour offset ('UTC-6', 'GMT+1').
 * Abbreviations beat offsets ('PST (UTC-8)' is Los Angeles, with daylight saving).
 * @param {unknown} name
 * @returns {string | null} null when the name names no zone, or several different ones
 */
export function zoneFromRoleName(name) {
  const named = new Set();
  const offsets = new Set();
  for (const m of String(name ?? '').toLowerCase().matchAll(/(?:utc|gmt)\s*[+\-−]\s*\d{1,2}(?::?00)?(?![\d:])|[a-z]+/g)) {
    const abbr = zoneFromAbbreviation(m[0]);
    if (abbr) named.add(abbr);
    else {
      const off = zoneFromOffset(m[0]);
      if (off) offsets.add(off);
    }
  }
  if (named.size > 1) named.delete('UTC');
  if (named.size === 1) return [...named][0];
  if (named.size === 0 && offsets.size === 1) return [...offsets][0];
  return null;
}

// ── Timezone roles and resolution ─────────────────────────────────────────────

/**
 * GuildSettings.timezoneRoles as a clean list: [{ roleId, zone }] with valid zones and each role
 * once, in stored order.
 * @param {unknown} raw
 * @returns {{ roleId: string, zone: string }[]}
 */
export function normalizeTimezoneRoles(raw) {
  if (!Array.isArray(raw)) return [];
  const seen = new Set();
  const out = [];
  for (const r of raw) {
    if (!r || typeof r.roleId !== 'string' || !/^\d{17,20}$/.test(r.roleId) || seen.has(r.roleId)) continue;
    if (!isValidTimeZone(r.zone)) continue;
    seen.add(r.roleId);
    out.push({ roleId: r.roleId, zone: r.zone });
  }
  return out;
}

/**
 * The zone a member's times are read in, and where it came from: their own zone (/timezone set),
 * else their timezone role (the highest-positioned one when they hold several), else the server
 * timezone, else UTC.
 * @param {{ personal?: string|null, memberRoleIds?: Iterable<string>, rolePosition?: (roleId: string) => number,
 *   settings?: { timezone?: string, timezoneRoles?: unknown } | null }} [input]
 * @returns {{ zone: string, source: 'personal'|'role'|'server'|'default', roleId: string|null, roleCount: number }}
 *   roleId is the winning timezone role the member holds (also when their own zone beats it)
 */
export function resolveTimezone({ personal = null, memberRoleIds = [], rolePosition = () => 0, settings = null } = {}) {
  const held = new Set(memberRoleIds);
  const links = normalizeTimezoneRoles(settings?.timezoneRoles).filter((l) => held.has(l.roleId));
  let best = null;
  for (const link of links) if (!best || rolePosition(link.roleId) > rolePosition(best.roleId)) best = link;
  const base = { roleId: best?.roleId ?? null, roleCount: links.length };
  if (isValidTimeZone(personal)) return { zone: personal, source: 'personal', ...base };
  if (best) return { zone: best.zone, source: 'role', ...base };
  if (isValidTimeZone(settings?.timezone)) return { zone: settings.timezone, source: 'server', ...base };
  return { zone: 'UTC', source: 'default', ...base };
}

// ── Display ───────────────────────────────────────────────────────────────────

const instantFormatters = new Map(); // zone → Intl.DateTimeFormat (bounded like offsetFormatters)

/**
 * An instant as wall-clock text in a zone: 'Fri, Oct 16, 2026, 7:00 PM'. Never throws.
 * @param {Date|number} date
 * @param {string} zone
 * @returns {string}
 */
export function formatInZone(date, zone) {
  const tz = isValidTimeZone(zone) ? zone : 'UTC';
  try {
    let f = instantFormatters.get(tz);
    if (!f) {
      f = new Intl.DateTimeFormat('en-US', {
        timeZone: tz, weekday: 'short', month: 'short', day: 'numeric', year: 'numeric', hour: 'numeric', minute: '2-digit',
      });
      if (instantFormatters.size >= 200) instantFormatters.delete(instantFormatters.keys().next().value);
      instantFormatters.set(tz, f);
    }
    return f.format(date instanceof Date ? date : new Date(Number(date)));
  } catch {
    return new Date(Number(date)).toISOString();
  }
}

/**
 * A zone for replies: 'America/Chicago (UTC-05:00)', 'UTC', or 'UTC+02:00' for an Etc/GMT offset zone.
 * @param {string} zone
 * @param {Date} [date] - the instant whose offset is shown (DST aware)
 * @returns {string}
 */
export function zoneLabel(zone, date = new Date()) {
  if (zone === 'UTC') return 'UTC';
  if (/^Etc\/GMT[+-]\d{1,2}$/.test(zone)) return utcOffsetLabel(zone, date);
  return `${zone} (${utcOffsetLabel(zone, date)})`;
}

/**
 * Where a member's zone came from, for replies (resolveTimezone's source).
 * @param {{ source: string, roleId: string|null }} resolved
 * @param {{ zoneFromInput?: boolean }} [opts] - true when the typed text named the zone
 * @returns {string}
 */
export function zoneSourceText(resolved, { zoneFromInput = false } = {}) {
  if (zoneFromInput) return 'the timezone in your text';
  switch (resolved.source) {
    case 'personal': return 'your saved timezone (`/timezone set`)';
    case 'role': return `your <@&${resolved.roleId}> role`;
    case 'server': return "this server's timezone";
    default: return 'UTC, because no timezone is set';
  }
}

// Echoes user input inside an error text: no code-span breakers, bounded.
const shownZone = (raw) => String(raw ?? '').replace(/[`\\]/g, '').trim().slice(0, 64) || '(empty)';

/**
 * Title and text for a parseZoneInput failure.
 * @param {unknown} raw - what was typed
 * @param {{ code: string, options?: string[] }} r
 * @returns {[string, string]}
 */
export function zoneInputError(raw, r) {
  switch (r.code) {
    case 'EMPTY':
      return ['Missing Timezone', 'Pick a timezone from the list, e.g. `America/Chicago`, or type an abbreviation like `CST`.'];
    case 'OFFSET':
      return ['Pick a City', 'Fixed offsets like `-06:00` ignore daylight saving time. Type the offset in the **zone** box to see the cities that use it, then pick one.'];
    case 'AMBIGUOUS':
      return ['Ambiguous Timezone', `\`${shownZone(raw)}\` can mean several timezones: ${r.options.map((z) => `\`${z}\``).join(', ')}. Pick the one you mean.`];
    case 'ABBREVIATION':
      return ['Pick a City', `\`${shownZone(raw)}\` is not a name I can be sure of. Pick a Region/City zone from the list, e.g. \`America/New_York\` or \`Europe/Paris\`.`];
    default:
      return ['Unknown Timezone', `\`${shownZone(raw)}\` is not a timezone I know. Pick one from the list, e.g. \`America/Chicago\`, \`Europe/London\` or \`Asia/Tokyo\`, or type an abbreviation like \`CST\`, \`EST\` or \`PST\`.`];
  }
}

// ── Timezone role templates (/reactionrole create template:timezones-*, the dashboard's panel templates) ───────

const tzRole = (name, zone, label, description) => Object.freeze({ name, zone, label, description });

// West to east. `name` is the Discord role (plain abbreviation, linked to `zone` explicitly, so ambiguous names such as IST
// are fine; a zone without daylight saving that shares one gets the region in brackets); `label` and `description` are
// the panel option (≤ 80 / ≤ 100 characters). At most 25, a dropdown's limit (template panels never use emoji reactions).
const HST = tzRole('HST', 'Pacific/Honolulu', 'Hawaii (HST)', 'Honolulu');
const AKST = tzRole('AKST', 'America/Anchorage', 'Alaska (AKST/AKDT)', 'Anchorage, Juneau');
const PST = tzRole('PST', 'America/Los_Angeles', 'Pacific (PST/PDT)', 'Los Angeles, Seattle, Vancouver');
const MST_AZ = tzRole('MST (AZ)', 'America/Phoenix', 'Arizona (MST, no daylight saving)', 'Phoenix, Tucson');
const MST = tzRole('MST', 'America/Denver', 'Mountain (MST/MDT)', 'Denver, Salt Lake City, Calgary');
const CST = tzRole('CST', 'America/Chicago', 'Central (CST/CDT)', 'Chicago, Dallas, Winnipeg');
const EST = tzRole('EST', 'America/New_York', 'Eastern (EST/EDT)', 'New York, Toronto, Miami');
const AST_PR = tzRole('AST (PR)', 'America/Puerto_Rico', 'Atlantic (AST, no daylight saving)', 'Puerto Rico, U.S. Virgin Islands, Santo Domingo');
const CHST = tzRole('ChST', 'Pacific/Guam', 'Chamorro (ChST)', 'Guam, Northern Mariana Islands');

export const TIMEZONE_ROLE_TEMPLATES = Object.freeze({
  'timezones-us': Object.freeze({
    label: 'Timezones: US',
    // The nine US time zones (Samoa to Chamorro), plus Arizona's Mountain time without daylight saving.
    roles: Object.freeze([
      tzRole('SST', 'Pacific/Pago_Pago', 'Samoa (SST)', 'American Samoa'),
      HST, AKST, PST, MST_AZ, MST, CST, EST, AST_PR, CHST,
    ]),
  }),
  'timezones-world': Object.freeze({
    label: 'Timezones: worldwide',
    roles: Object.freeze([
      HST, AKST, PST, MST_AZ, MST,
      tzRole('CST (MX)', 'America/Mexico_City', 'Mexico (CST, no daylight saving)', 'Mexico City, Guadalajara, Monterrey'),
      CST, EST, AST_PR,
      tzRole('AST', 'America/Halifax', 'Atlantic (AST/ADT)', 'Halifax, Bermuda'),
      tzRole('BRT', 'America/Sao_Paulo', 'Brasília (BRT)', 'São Paulo, Rio de Janeiro'),
      tzRole('GMT', 'Europe/London', 'UK & Ireland (GMT/BST)', 'London, Dublin, Lisbon'),
      tzRole('CET', 'Europe/Berlin', 'Central Europe (CET/CEST)', 'Berlin, Paris, Rome, Madrid'),
      tzRole('EET', 'Europe/Athens', 'Eastern Europe (EET/EEST)', 'Athens, Helsinki, Kyiv'),
      tzRole('MSK', 'Europe/Moscow', 'Moscow (MSK)', 'Moscow, Istanbul'),
      tzRole('GST', 'Asia/Dubai', 'Gulf (GST)', 'Dubai, Abu Dhabi'),
      tzRole('PKT', 'Asia/Karachi', 'Pakistan (PKT)', 'Karachi, Lahore, Islamabad'),
      tzRole('IST', 'Asia/Kolkata', 'India (IST)', 'Mumbai, Delhi, Bengaluru'),
      tzRole('ICT', 'Asia/Bangkok', 'Indochina (ICT)', 'Bangkok, Jakarta, Hanoi'),
      tzRole('SGT', 'Asia/Singapore', 'Singapore & China (UTC+8)', 'Singapore, Beijing, Manila, Perth'),
      tzRole('JST', 'Asia/Tokyo', 'Japan & Korea (JST/KST)', 'Tokyo, Seoul'),
      CHST,
      tzRole('AEST', 'Australia/Sydney', 'Sydney (AEST/AEDT)', 'Sydney, Melbourne'),
      tzRole('NZST', 'Pacific/Auckland', 'New Zealand (NZST/NZDT)', 'Auckland, Wellington'),
    ]),
  }),
});

// The zones of each template's panel before a template gained roles; upgradeTimezonePanels adds the new roles to a panel
// whose zones are exactly one of these. Never edit a set: append one for the next change.
export const TIMEZONE_TEMPLATE_PAST_ZONES = Object.freeze({
  'timezones-us': Object.freeze([
    Object.freeze(['Pacific/Honolulu', 'America/Anchorage', 'America/Los_Angeles', 'America/Denver', 'America/Chicago',
      'America/New_York']),
  ]),
  'timezones-world': Object.freeze([
    Object.freeze(['Pacific/Honolulu', 'America/Anchorage', 'America/Los_Angeles', 'America/Denver', 'America/Chicago',
      'America/New_York', 'America/Halifax', 'America/Sao_Paulo', 'Europe/London', 'Europe/Berlin', 'Europe/Athens',
      'Europe/Moscow', 'Asia/Dubai', 'Asia/Kolkata', 'Asia/Bangkok', 'Asia/Singapore', 'Asia/Tokyo', 'Australia/Sydney',
      'Pacific/Auckland']),
  ]),
});

/** The text of a timezone panel: Unique mode (one role at a time) and a dropdown. */
export const TIMEZONE_PANEL = Object.freeze({
  type: 'UNIQUE',
  layout: 'SELECT',
  title: '🌍 Timezone Roles',
  description: 'Pick your timezone so `/timestamp` reads your times right. You can hold one timezone role at a time; '
    + 'pick another to switch.',
  placeholder: 'Pick your timezone',
});
