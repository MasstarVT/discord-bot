// Calendar export tests (utils/calendar.js). Pure: the Google Calendar and Outlook.com links (fields, UTC times, the 512
// character cap of a Discord link button with emoji-heavy text) and the .ics file (escaping, folding at 75 octets, CRLF).
import { test } from 'node:test';
import assert from 'node:assert/strict';

const cal = await import('../src/utils/calendar.js');

const START = new Date('2026-11-05T19:00:00.000Z');
const END = new Date('2026-11-05T20:30:00.000Z');
const URL_BACK = 'https://discord.com/channels/111111111111111111/222222222222222222/333333333333333333';
const entry = (over = {}) => ({
  uid: 'ev1@events.discord-bot', title: 'Movie night', description: 'Bring snacks', location: 'Main hall',
  start: START, end: END, url: URL_BACK, sequence: 7, ...over,
});

test('Google Calendar link: template action, UTC dates, title, location, details ending in the link back', () => {
  const url = new URL(cal.googleCalendarUrl(entry()));
  assert.equal(url.origin + url.pathname, 'https://calendar.google.com/calendar/render');
  assert.equal(url.searchParams.get('action'), 'TEMPLATE');
  assert.equal(url.searchParams.get('dates'), '20261105T190000Z/20261105T203000Z');
  assert.equal(url.searchParams.get('text'), 'Movie night');
  assert.equal(url.searchParams.get('location'), 'Main hall');
  assert.equal(url.searchParams.get('details'), `Bring snacks\n\n${URL_BACK}`);
});

test('Outlook link: add-event compose, ISO UTC times, subject, location, body', () => {
  const url = new URL(cal.outlookCalendarUrl(entry()));
  assert.equal(url.origin + url.pathname, 'https://outlook.live.com/calendar/0/deeplink/compose');
  assert.equal(url.searchParams.get('path'), '/calendar/action/compose');
  assert.equal(url.searchParams.get('rru'), 'addevent');
  assert.equal(url.searchParams.get('startdt'), '2026-11-05T19:00:00Z');
  assert.equal(url.searchParams.get('enddt'), '2026-11-05T20:30:00Z');
  assert.equal(url.searchParams.get('subject'), 'Movie night');
  assert.equal(url.searchParams.get('location'), 'Main hall');
  assert.equal(url.searchParams.get('body'), `Bring snacks\n\n${URL_BACK}`);
});

test('links: empty fields are left out, and a plain entry is not cut', () => {
  const url = new URL(cal.googleCalendarUrl(entry({ description: '', location: '', url: '' })));
  assert.equal(url.searchParams.has('location'), false);
  assert.equal(url.searchParams.has('details'), false);
  assert.equal(url.searchParams.get('text'), 'Movie night');
});

test('links stay within 512 characters with emoji-heavy text; the description is cut before the link back', () => {
  const emoji = '🎉';
  const heavy = entry({ title: emoji.repeat(50), description: `${'🍿 popcorn '.repeat(100)}`, location: '🏠'.repeat(50) });
  for (const build of [cal.googleCalendarUrl, cal.outlookCalendarUrl]) {
    const link = build(heavy);
    assert.ok(link.length <= cal.LINK_URL_MAX, `${link.length} characters`);
    const url = new URL(link); // decodes cleanly: no character was split
    const title = url.searchParams.get('text') ?? url.searchParams.get('subject');
    assert.ok(title.length > 0 && title.endsWith('…'), 'the title is kept, cut');
    assert.equal([...title.slice(0, -1)].every((c) => c === emoji), true);
  }

  const long = entry({ description: 'word '.repeat(400) });
  for (const build of [cal.googleCalendarUrl, cal.outlookCalendarUrl]) {
    const link = build(long);
    assert.ok(link.length <= cal.LINK_URL_MAX);
    const url = new URL(link);
    const details = url.searchParams.get('details') ?? url.searchParams.get('body');
    assert.ok(details.endsWith(`…\n\n${URL_BACK}`), 'the description is cut, the link back is whole');
  }

  const tiny = cal.googleCalendarUrl(entry({ description: 'x'.repeat(1000) }), 200);
  assert.ok(tiny.length <= 200);
});

test('fitEncoded: within the budget, whole characters only, ellipsis when cut, lone surrogates repaired', () => {
  assert.equal(cal.fitEncoded('hello', 5), 'hello');
  const cut = cal.fitEncoded('a😀b😀c😀d😀', 30);
  assert.ok(encodeURIComponent(cut).length <= 30);
  assert.ok(cut.endsWith('…'));
  assert.doesNotThrow(() => encodeURIComponent(cut));
  assert.equal(cal.fitEncoded('abcdef', 3), '');
  assert.doesNotThrow(() => encodeURIComponent(cal.fitEncoded('ab\uD83D', 100)));
  assert.equal(cal.fitEncoded(null, 10), '');
});

test('icsEscape: backslash first, then semicolons, commas and line breaks', () => {
  assert.equal(cal.icsEscape('a\\b;c,d\r\ne\nf'), 'a\\\\b\\;c\\,d\\ne\\nf');
  assert.equal(cal.icsEscape(undefined), '');
});

test('icsFold: lines of at most 75 octets, never inside a UTF-8 character, unfolding gives the line back', () => {
  const line = `SUMMARY:${'🎉 party '.repeat(40)}`;
  const folded = cal.icsFold(line);
  const parts = folded.split('\r\n');
  assert.ok(parts.length > 1);
  for (const [i, part] of parts.entries()) {
    assert.ok(Buffer.byteLength(part, 'utf8') <= 75, `line ${i} is ${Buffer.byteLength(part, 'utf8')} octets`);
    if (i > 0) assert.equal(part[0], ' ');
    assert.equal(Buffer.from(part, 'utf8').toString('utf8'), part, 'no split character');
    assert.equal(/[\uD800-\uDBFF]$|^ ?[\uDC00-\uDFFF]/.test(part), false, 'no split surrogate pair');
  }
  assert.equal(folded.replace(/\r\n /g, ''), line);
  assert.equal(cal.icsFold('SHORT:x'), 'SHORT:x');
});

test('buildIcs: one VEVENT, CRLF only, UTC times, stable UID and SEQUENCE, description ends with the link back', () => {
  const ics = cal.buildIcs(entry({ description: 'Snacks, drinks; fun' }), { now: Date.UTC(2026, 9, 22, 12, 0, 0) });
  assert.ok(ics.startsWith('BEGIN:VCALENDAR\r\nVERSION:2.0\r\n'));
  assert.ok(ics.endsWith('END:VEVENT\r\nEND:VCALENDAR\r\n'));
  assert.equal(/[^\r]\n/.test(ics), false, 'no bare line feeds');
  const lines = ics.replace(/\r\n /g, '').split('\r\n');
  for (const want of [
    'METHOD:PUBLISH', 'UID:ev1@events.discord-bot', 'DTSTAMP:20261022T120000Z', 'DTSTART:20261105T190000Z',
    'DTEND:20261105T203000Z', 'SEQUENCE:7', 'SUMMARY:Movie night', 'LOCATION:Main hall', `URL:${URL_BACK}`,
    `DESCRIPTION:Snacks\\, drinks\\; fun\\n\\n${URL_BACK}`, 'STATUS:CONFIRMED',
  ]) assert.ok(lines.includes(want), want);
  for (const raw of ics.split('\r\n')) assert.ok(Buffer.byteLength(raw, 'utf8') <= 75);

  const bare = cal.buildIcs(entry({ description: '', location: '', url: '', sequence: undefined }));
  assert.equal(/^(DESCRIPTION|LOCATION|URL):/m.test(bare), false);
  assert.match(bare, /\r\nSEQUENCE:0\r\n/);
});
