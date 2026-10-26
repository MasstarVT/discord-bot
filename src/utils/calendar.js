// "Add to calendar" for one timed event: links that open a prefilled entry in Google Calendar or Outlook.com, and an
// iCalendar file (.ics, RFC 5545) for Apple Calendar, Outlook desktop and the rest. Pure: nothing runs at import. Times are
// always written in UTC (`Z`), so every calendar shows the entry in its owner's own zone.

/** Discord refuses a link button whose URL is longer than this, and with it the whole message. */
export const LINK_URL_MAX = 512;

const ELLIPSIS = '…';
const GOOGLE_URL = 'https://calendar.google.com/calendar/render';
// The deeplink form the add-to-calendar-button library uses (outlook.office.com takes the same for work accounts).
const OUTLOOK_URL = 'https://outlook.live.com/calendar/0/deeplink/compose';

/**
 * @typedef {object} CalendarEntry
 * @property {string} uid stable across edits, so importing the file again updates the entry instead of adding another
 * @property {string} title
 * @property {string} [description] plain text
 * @property {string} [location]
 * @property {Date} start
 * @property {Date} end
 * @property {string} [url] a link back to where the event was announced
 * @property {number} [sequence] grows whenever the event changes (the iCalendar SEQUENCE)
 */

const wellFormed = (s) => String(s ?? '').toWellFormed();
const encodedLength = (s) => encodeURIComponent(s).length;
/** '20261105T190000Z' */
const compactUtc = (date) => new Date(date).toISOString().replace(/\.\d{3}Z$/, 'Z').replace(/[-:]/g, '');
/** '2026-11-05T19:00:00Z' */
const isoUtc = (date) => new Date(date).toISOString().replace(/\.\d{3}Z$/, 'Z');

/**
 * The longest start of `text` (whole characters) whose URL encoding is at most `budget` characters long, ending in an
 * ellipsis when it was cut. '' when not even that fits.
 * @param {unknown} text
 * @param {number} budget
 * @returns {string}
 */
export function fitEncoded(text, budget) {
  const s = wellFormed(text);
  if (encodedLength(s) <= budget) return s;
  const room = budget - encodedLength(ELLIPSIS);
  let used = 0;
  let out = '';
  for (const ch of s) {
    const n = encodedLength(ch);
    if (used + n > room) break;
    used += n;
    out += ch;
  }
  out = out.trimEnd();
  return out ? `${out}${ELLIPSIS}` : '';
}

/** The description, then the link back: the description is cut first, the link only goes when it cannot fit at all. */
function fitDetails(entry, budget) {
  const url = entry.url ? wellFormed(entry.url) : '';
  const tail = url ? encodedLength(`\n\n${url}`) : 0;
  if (!url || tail > budget) return fitEncoded(entry.description, budget);
  const description = fitEncoded(entry.description, budget - tail);
  return description ? `${description}\n\n${url}` : url;
}

/**
 * `head` (already encoded) plus each `[key, value]` in order, every value cut to the room left so the whole URL stays
 * within `max`. A value is a string, or a function given the room left that returns one; an empty value is left out.
 */
function linkWithin(head, pairs, max) {
  let url = head;
  for (const [key, value] of pairs) {
    const room = max - url.length - key.length - 2; // '&' and '='
    const v = typeof value === 'function' ? value(room) : fitEncoded(value, room);
    if (v) url += `&${key}=${encodeURIComponent(v)}`;
  }
  return url;
}

/**
 * @param {CalendarEntry} entry
 * @param {number} [max]
 * @returns {string} a Google Calendar "new event" link of at most `max` characters
 */
export function googleCalendarUrl(entry, max = LINK_URL_MAX) {
  return linkWithin(`${GOOGLE_URL}?action=TEMPLATE&dates=${compactUtc(entry.start)}/${compactUtc(entry.end)}`, [
    ['text', entry.title],
    ['location', entry.location],
    ['details', (room) => fitDetails(entry, room)],
  ], max);
}

/**
 * @param {CalendarEntry} entry
 * @param {number} [max]
 * @returns {string} an Outlook.com "new event" link of at most `max` characters
 */
export function outlookCalendarUrl(entry, max = LINK_URL_MAX) {
  return linkWithin(`${OUTLOOK_URL}?path=%2Fcalendar%2Faction%2Fcompose&rru=addevent&startdt=${isoUtc(entry.start)}&enddt=${isoUtc(entry.end)}`, [
    ['subject', entry.title],
    ['location', entry.location],
    ['body', (room) => fitDetails(entry, room)],
  ], max);
}

/** RFC 5545 TEXT: backslash first, then `;`, `,` and line breaks. */
export function icsEscape(text) {
  return wellFormed(text)
    .replace(/\\/g, '\\\\')
    .replace(/;/g, '\\;')
    .replace(/,/g, '\\,')
    .replace(/\r\n|\r|\n/g, '\\n');
}

/** Folds a content line at 75 octets (RFC 5545 §3.1) without splitting a UTF-8 character; continuations start with a space. */
export function icsFold(line) {
  const out = [];
  let cur = '';
  let bytes = 0;
  for (const ch of line) {
    const n = Buffer.byteLength(ch, 'utf8');
    if (bytes + n > (out.length ? 74 : 75)) {
      out.push(cur);
      cur = '';
      bytes = 0;
    }
    cur += ch;
    bytes += n;
  }
  out.push(cur);
  return out.join('\r\n ');
}

/**
 * One VEVENT in a VCALENDAR, CRLF line endings, lines folded.
 * @param {CalendarEntry} entry
 * @param {{ now?: number|Date, prodId?: string }} [opts] `now` is the DTSTAMP
 * @returns {string}
 */
export function buildIcs(entry, { now = Date.now(), prodId = '-//discord-bot//Events//EN' } = {}) {
  const sequence = Number.isFinite(entry.sequence) ? Math.max(0, Math.floor(entry.sequence)) : 0;
  const url = entry.url ? wellFormed(entry.url) : '';
  const description = [wellFormed(entry.description).trim(), url].filter(Boolean).join('\n\n');
  const lines = [
    'BEGIN:VCALENDAR', 'VERSION:2.0', `PRODID:${prodId}`, 'CALSCALE:GREGORIAN', 'METHOD:PUBLISH',
    'BEGIN:VEVENT',
    `UID:${icsEscape(entry.uid)}`,
    `DTSTAMP:${compactUtc(now)}`,
    `DTSTART:${compactUtc(entry.start)}`,
    `DTEND:${compactUtc(entry.end)}`,
    `SEQUENCE:${sequence}`,
    `SUMMARY:${icsEscape(entry.title)}`,
  ];
  if (description) lines.push(`DESCRIPTION:${icsEscape(description)}`);
  if (entry.location) lines.push(`LOCATION:${icsEscape(entry.location)}`);
  if (url) lines.push(`URL:${url.replace(/[\r\n]/g, '')}`);
  lines.push('STATUS:CONFIRMED', 'END:VEVENT', 'END:VCALENDAR');
  return `${lines.map(icsFold).join('\r\n')}\r\n`;
}
