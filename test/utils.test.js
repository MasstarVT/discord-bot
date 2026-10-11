// Offline tests for the foundation utilities in src/utils (placeholders, text, durations, time,
// message templates, safe regex, safe fetch, interaction helpers, deletion tracker, image sizes,
// member fetching and the paginator). Nothing here touches Discord, Postgres, Redis or the
// network. Run with `npm test`.
// Nothing here opens Redis or Prisma, so there is nothing to disconnect in after(). The discord.js
// import is for the real InteractionCollector the paginator and confirm-dialog cases drive.
process.env.NODE_ENV      ??= 'test';
process.env.DISCORD_TOKEN ??= 'test-token';
process.env.CLIENT_ID     ??= '000000000000000000';
process.env.DATABASE_URL  ??= 'postgresql://test:test@127.0.0.1:1/test';
process.env.REDIS_URL     ??= 'redis://127.0.0.1:1';

import { describe, it, afterEach, mock } from 'node:test';
import assert from 'node:assert/strict';
import dns from 'node:dns';
import http from 'node:http';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import zlib from 'node:zlib';
import {
  EmbedBuilder,
  InteractionCollector,
  InteractionType,
  ComponentType,
  PermissionFlagsBits,
  PermissionsBitField,
} from 'discord.js';

import {
  renderPlaceholders,
  renderPlaceholdersDetailed,
  parsePlaceholders,
  placeholderHelp,
  PLACEHOLDERS,
  PLACEHOLDER_LIST,
} from '../src/utils/placeholderParser.js';
import {
  clampText, chunkLines, joinWithLimit, ordinal, formatNumber, compactNumber, progressBar, sanitizeName,
} from '../src/utils/text.js';
import { parseDuration, formatDuration, formatDurationLong, MAX_DURATION_MS } from '../src/utils/parseDuration.js';
import {
  WEEKDAY_NAMES, isValidTimeZone, zonedParts, zonedToUtc, localDateKey, daysInMonth, parseTimeOfDay,
  formatTimeOfDay, parseWeekdays, nextTimeOfDay, nextIntervalRun, parseDateInput, parseBirthday, discordTimestamp,
} from '../src/utils/time.js';
import {
  LIMITS, TEMPLATE_FIELD_IDS, MODAL_INPUT_MAX, emptyTemplate, normalizeTemplate, normalizeLegacyEmbed,
  validateTemplate, EMPTY_MESSAGE_ERROR, renderEmbed, renderTemplate, specFromEmbed, parseColor, formatColor, isHttpUrl,
  buildTemplateModal, parseTemplateModal, templateToJson, templateFromJson,
} from '../src/utils/customEmbed.js';
import {
  validateRegex, compileSafeRegex, safeRegexTest, safeRegexFirstMatch, isQuarantined, escapeRegex, containsWord,
} from '../src/utils/safeRegex.js';
import {
  SafeFetchError, DEFAULT_USER_AGENT, isBlockedAddress, assertPublicUrl, safeFetch, safeFetchJson, safeFetchText,
  safeFetchImage,
} from '../src/utils/safeFetch.js';
import {
  isInteractionGone, safeDefer, safeReply, replyError, replySuccess, permissionNames, missingBotPermissions,
  requireBotPermissions, checkChannelUsable, checkAssignableRole, pingableRoleIds, respondAutocomplete, awaitConfirm,
} from '../src/utils/interactionHelpers.js';
import { markBotDeletion, wasDeletedByBot } from '../src/utils/deletionTracker.js';
import { readImageSize, assertImageDimensions, DEFAULT_MAX_SIDE, DEFAULT_MAX_PIXELS } from '../src/utils/imageSize.js';
import { fetchAllMembers, FETCH_ALL_MAX_MEMBERS } from '../src/utils/guildMembers.js';
import { paginate } from '../src/utils/paginator.js';
import { successEmbed, errorEmbed, infoEmbed, warnEmbed, neutralEmbed, applyGuildColor, guildEmbed } from '../src/utils/embedBuilder.js';
import { formatError } from '../src/utils/errorFormatter.js';

// Embed colours, copied from COLORS in src/config/constants.js (this file imports only src/utils, node:* and discord.js).
const COLORS = { SUCCESS: 0x57f287, INFO: 0x5865f2, NEUTRAL: 0x99aab5 };
const D = (s) => new Date(s);
const tick = (ms = 20) => new Promise((resolve) => setTimeout(resolve, ms));

// Plain fakes for placeholders.
const fakeUser = (over = {}) => ({
  id: '1', username: 'bob', discriminator: '0', bot: false, createdTimestamp: 0,
  displayAvatarURL: () => 'https://cdn/a.png', ...over,
});
const fakeGuild = (over = {}) => ({
  id: '9', name: 'G', memberCount: 1001, ownerId: '2', iconURL: () => null, ...over,
});

// ── Placeholders ─────────────────────────────────────────────────────────────

describe('placeholders', () => {
  const user = fakeUser();
  const guild = fakeGuild();

  it('renders {user} and {user.mention} as a mention, every occurrence', () => {
    assert.equal(renderPlaceholders('{user} {user.mention}', { user }), '<@1> <@1>');
    assert.equal(renderPlaceholders('{user} {user}', { user }), '<@1> <@1>');
  });

  it('never re-expands a substituted value, and $& stays literal', () => {
    assert.equal(renderPlaceholders('{user.name} in {server}', { user: fakeUser({ username: '{server}' }), guild }), '{server} in G');
    assert.equal(renderPlaceholders('{x}', { vars: { x: '$&' } }), '$&');
    assert.equal(renderPlaceholders('{x}', { vars: { x: '$1 $$' } }), '$1 $$');
  });

  it('supports random tokens', () => {
    assert.equal(renderPlaceholders('{random:5:5}', {}), '5');
    for (let i = 0; i < 50; i++) {
      const v = Number(renderPlaceholders('{random:9:1}', {}));
      assert.ok(v >= 1 && v <= 9);
      const w = Number(renderPlaceholders('{random}', {}));
      assert.ok(w >= 0 && w <= 10);
    }
  });

  it('resolves argument tokens', () => {
    const ctx = { args: ['a', 'b', 'c'] };
    assert.equal(renderPlaceholders('{1}', ctx), 'a');
    assert.equal(renderPlaceholders('{2:def}', ctx), 'b');
    assert.equal(renderPlaceholders('{4:def}', ctx), 'def');
    assert.equal(renderPlaceholders('{1...}', ctx), 'a b c');
    assert.equal(renderPlaceholders('{...2}', ctx), 'a b');
    assert.equal(renderPlaceholders('{2...3}', ctx), 'b c');
    assert.equal(renderPlaceholders('{...}', ctx), 'a b c');
    assert.equal(renderPlaceholders('{2...}', ctx), 'b c');
    const r = renderPlaceholdersDetailed('{4}', ctx);
    assert.equal(r.text, '');
    assert.deepEqual(r.missingArgs, [4]);
    assert.deepEqual(renderPlaceholdersDetailed('{4:x}', ctx).missingArgs, []);
    // An empty argument that exists is not missing.
    assert.deepEqual(renderPlaceholdersDetailed('{1}', { args: [''] }).missingArgs, []);
  });

  it('leaves argument tokens untouched without ctx.args and lists them as unknown', () => {
    const r = renderPlaceholdersDetailed('{1}', {});
    assert.equal(r.text, '{1}');
    assert.deepEqual(r.unknownTokens, ['{1}']);
  });

  it('lets vars override built-ins and normalises var keys', () => {
    assert.equal(renderPlaceholders('{user}', { user, vars: { user: 'x' } }), 'x');
    assert.equal(renderPlaceholders('{level}', { vars: { '{Level}': 5 } }), '5');
    assert.equal(renderPlaceholders('{a}{b}', { vars: { a: () => 'f', b: () => { throw new Error('boom'); } } }), 'f');
    assert.equal(renderPlaceholders('{n}', { vars: { n: null } }), '');
  });

  it('reports unknown tokens with braces and original case, unique and in order', () => {
    const r = renderPlaceholdersDetailed('{nope} {Nope} {nope}', {});
    assert.equal(r.text, '{nope} {Nope} {nope}');
    assert.deepEqual(r.unknownTokens, ['{nope}', '{Nope}']);
    assert.deepEqual(renderPlaceholdersDetailed('{nope}').unknownTokens, ['{nope}']);
  });

  it('leaves a known token without its context untouched and unreported', () => {
    const r = renderPlaceholdersDetailed('{channel} {user} {server}', {});
    assert.equal(r.text, '{channel} {user} {server}');
    assert.deepEqual(r.unknownTokens, []);
  });

  it('clamps with maxLength', () => {
    assert.equal(renderPlaceholders('abcdefgh', {}, { maxLength: 5 }), 'abcd…');
  });

  it('handles empty and non-string templates', () => {
    assert.equal(renderPlaceholders(null), '');
    assert.equal(renderPlaceholders(undefined), '');
    assert.equal(renderPlaceholders(''), '');
    assert.equal(renderPlaceholders(42), '42');
    assert.equal(renderPlaceholders('{user}', null), '{user}');
  });

  it('renders the built-in tokens', () => {
    const member = { user, displayName: 'Bobby', joinedTimestamp: 3_000_000, guild, displayAvatarURL: () => 'https://cdn/a.png' };
    const ctx = { member, channel: { id: '5', name: 'general', type: 0 } };
    assert.equal(renderPlaceholders('{user.name}|{user.id}|{user.bot}|{user.display_name}', ctx), 'bob|1|False|Bobby');
    assert.equal(renderPlaceholders('{user.avatar}|{user.avatar_url}', ctx), 'https://cdn/a.png|https://cdn/a.png');
    assert.equal(renderPlaceholders('{user.created}|{user.created_at}', ctx), '<t:0:R>|<t:0:D>');
    assert.equal(renderPlaceholders('{user.joined}|{user.joined_at}', ctx), '<t:3000:R>|<t:3000:D>');
    assert.equal(renderPlaceholders('{server}|{server.name}|{server.id}|{server.owner}|{server.owner_id}', ctx), 'G|G|9|<@2>|2');
    assert.equal(renderPlaceholders('{server.member_count}|{server.member_count.ordinal}', ctx), '1,001|1,001st');
    assert.equal(renderPlaceholders('{server.icon_url}', ctx), '');
    assert.equal(renderPlaceholders('{channel}|{channel.name}|{channel.id}|{channel.type}', ctx), '<#5>|general|5|text');
  });

  it('keeps the legacy parsePlaceholders wrapper working', () => {
    const member = { user, guild };
    assert.equal(parsePlaceholders('{user.tag} {server.member_count.ordinal}', member, guild), 'bob 1,001st');
    assert.equal(parsePlaceholders('{user.tag}', null, guild), '{user.tag}');
    assert.equal(parsePlaceholders('', member, guild), '');
  });

  it('exposes the help text and the token table', () => {
    assert.ok(PLACEHOLDER_LIST.length <= 1024);
    assert.equal(PLACEHOLDER_LIST, placeholderHelp(['user', 'server']));
    const lengths = { user: 404, server: 453, channel: 88, random: 77, args: 147 };
    for (const [group, len] of Object.entries(lengths)) {
      const text = placeholderHelp([group]);
      assert.ok(text.length <= 1024, group);
      assert.equal(text.length, len, `${group} length`);
    }
    assert.equal(PLACEHOLDER_LIST.length, 859);
    assert.ok(Object.isFrozen(PLACEHOLDERS));
    for (const token of ['{user}', '{user.mention}', '{server.member_count.ordinal}', '{channel.type}', '{random}', '{random:x:y}']) {
      assert.equal(typeof PLACEHOLDERS[token], 'string', token);
    }
    assert.equal(placeholderHelp(['nope']), '');
    assert.ok(placeholderHelp(null).startsWith('**User**'));
  });
});

// ── Text ─────────────────────────────────────────────────────────────────────

describe('text', () => {
  it('ordinal', () => {
    const expected = { 1: '1st', 2: '2nd', 3: '3rd', 4: '4th', 11: '11th', 12: '12th', 13: '13th', 21: '21st', 22: '22nd', 23: '23rd', 101: '101st', 111: '111th', 1001: '1,001st' };
    for (const [n, text] of Object.entries(expected)) assert.equal(ordinal(Number(n)), text);
  });

  it('clampText never splits a surrogate pair and respects the maximum', () => {
    assert.equal(clampText('aaa😀bb', 5), 'aaa…');
    for (let max = 1; max < 12; max++) {
      const out = clampText('héllo 😀😀😀 wörld 😀', max);
      assert.ok(out.length <= max, `max ${max}`);
      assert.ok(!/[\uD800-\uDBFF](?![\uDC00-\uDFFF])/.test(out), `lone surrogate at max ${max}`);
    }
    assert.equal(clampText(null, 5), '');
    assert.equal(clampText('abc', 0), '');
    assert.equal(clampText('abc', 10), 'abc');
    assert.equal(clampText('abcdef', 1), '…');
    assert.equal(clampText('abcdef', 2), 'a…');
    assert.equal(clampText('abcdef', Infinity), 'abcdef');
  });

  it('compactNumber', () => {
    assert.equal(compactNumber(999_950), '1M');
    assert.equal(compactNumber(1500), '1.5K');
    assert.equal(compactNumber(999), '999');
    assert.equal(compactNumber(2_500_000), '2.5M');
    assert.equal(compactNumber(-1500), '-1.5K');
    assert.equal(compactNumber(NaN), '0');
    assert.equal(compactNumber(1000), '1K');
  });

  it('formatNumber', () => {
    assert.equal(formatNumber(1234567), '1,234,567');
    assert.equal(formatNumber(10n ** 12n), '1,000,000,000,000');
    assert.equal(formatNumber('nope'), '0');
  });

  it('joinWithLimit never cuts an item', () => {
    const mention = '<@123456789012345678>';
    const out = joinWithLimit(Array.from({ length: 300 }, () => mention), { max: 1024 });
    assert.ok(out.length <= 1024);
    const parts = out.split(', ');
    for (const p of parts.slice(0, -1)) assert.equal(p, mention);
    assert.match(parts.at(-1), /^… and \d+ more$/);
    assert.equal(joinWithLimit([], {}), '');
    assert.equal(joinWithLimit(['a', 'b'], { max: 10 }), 'a, b');
    assert.equal(joinWithLimit(['a'.repeat(50)], { max: 20 }).length <= 20, true);
  });

  it('chunkLines keeps every chunk within the limit', () => {
    const lines = Array.from({ length: 200 }, (_, i) => `line ${String(i).padStart(3, '0')} `.padEnd(30, 'x'));
    const chunks = chunkLines(lines);
    assert.ok(chunks.length > 1);
    for (const c of chunks) assert.ok(c.length <= 1024);
    assert.equal(chunks.join('\n'), lines.join('\n'));
    assert.deepEqual(chunkLines([]), []);
    assert.deepEqual(chunkLines('nope'), []);
    assert.ok(chunkLines(['x'.repeat(5000)])[0].length <= 1024);
  });

  it('progressBar and sanitizeName', () => {
    assert.equal(progressBar(0.5, 10), '▰▰▰▰▰▱▱▱▱▱');
    assert.equal(progressBar(NaN, 4), '▱▱▱▱');
    assert.equal(progressBar(5, 4), '▰▰▰▰');
    assert.equal(sanitizeName('  Café Rules!! '), 'cafe-rules');
    assert.equal(sanitizeName('!!!'), 'item');
    assert.equal(sanitizeName('a'.repeat(50)).length, 32);
  });
});

// ── parseDuration ────────────────────────────────────────────────────────────

describe('parseDuration', () => {
  it('parses decimals, months, years and compound input', () => {
    assert.equal(parseDuration('1.5h'), 5_400_000);
    assert.equal(parseDuration('0.5d'), 43_200_000);
    assert.equal(parseDuration('1mo'), 2_592_000_000);
    assert.equal(parseDuration('2h30m'), 9_000_000);
    assert.equal(parseDuration('5 minutes and 3s'), 303_000);
    assert.equal(parseDuration('1y'), 31_536_000_000);
    assert.equal(parseDuration('1h, 30m'), 5_400_000);
    assert.equal(parseDuration('7d'), 604_800_000);
  });

  it('rejects anything that is not entirely a duration', () => {
    for (const bad of ['abc', '5x', '5ms', '30', '1h 30', '99999999999d', '', '   ', '1h garbage', '-5m', null, undefined, 5]) {
      assert.equal(parseDuration(bad), null, String(bad));
    }
    assert.equal(parseDuration('x'.repeat(101)), null);
  });

  it('treats "1 month" as a month, never a minute', () => {
    assert.notEqual(parseDuration('1 month'), 60_000);
    assert.equal(parseDuration('1 month'), 2_592_000_000);
    assert.equal(parseDuration('2 months'), 5_184_000_000);
  });

  it('caps at MAX_DURATION_MS', () => {
    assert.equal(MAX_DURATION_MS, 3_153_600_000_000);
    assert.equal(parseDuration('100y'), MAX_DURATION_MS);
    assert.equal(parseDuration('101y'), null);
  });

  it('formats durations', () => {
    assert.equal(formatDurationLong(9_000_000), '2 hours 30 minutes');
    assert.equal(formatDurationLong(2_592_000_000), '1 month');
    assert.equal(formatDurationLong(90_061_000), '1 day 1 hour 1 minute 1 second');
    assert.equal(formatDurationLong(0), 'Permanent');
    assert.equal(formatDurationLong(500), '0 seconds');
    assert.equal(formatDuration(9_000_000), '2h 30m');
    assert.equal(formatDuration(0), 'Permanent');
  });
});

// ── Time ─────────────────────────────────────────────────────────────────────

describe('time', () => {
  it('validates timezones', () => {
    assert.equal(isValidTimeZone('Europe/London'), true);
    assert.equal(isValidTimeZone('utc'), true);
    assert.equal(isValidTimeZone('Mars/Base'), false);
    assert.equal(isValidTimeZone(''), false);
    assert.equal(isValidTimeZone(null), false);
  });

  it('never reports hour 24 and treats an invalid zone as UTC', () => {
    assert.equal(zonedParts(D('2026-01-01T00:00:00Z'), 'UTC').hour, 0);
    assert.equal(zonedParts(D('2026-01-01T00:00:00Z'), 'America/New_York').hour, 19);
    assert.deepEqual(zonedParts(D('2026-03-02T10:20:30Z'), 'Mars/Base'), zonedParts(D('2026-03-02T10:20:30Z'), 'UTC'));
    assert.equal(zonedParts(D('2026-03-02T10:20:30Z'), 'UTC').weekday, 1);
    assert.equal(localDateKey(D('2026-03-02T23:30:00Z'), 'Asia/Tokyo'), '2026-03-03');
    assert.equal(daysInMonth(2024, 2), 29);
    assert.equal(daysInMonth(2026, 2), 28);
  });

  it('zonedToUtc handles gaps and overlaps', () => {
    const iso = (local, tz) => zonedToUtc(local, tz).toISOString();
    assert.equal(iso({ year: 2026, month: 3, day: 29, hour: 1, minute: 30 }, 'Europe/London'), '2026-03-29T01:00:00.000Z');
    assert.equal(iso({ year: 2026, month: 10, day: 25, hour: 1, minute: 30 }, 'Europe/London'), '2026-10-25T00:30:00.000Z');
    assert.equal(iso({ year: 2026, month: 3, day: 8, hour: 2, minute: 30 }, 'America/New_York'), '2026-03-08T07:00:00.000Z');
    assert.equal(iso({ year: 2026, month: 7, day: 4, hour: 9, minute: 0 }, 'America/New_York'), '2026-07-04T13:00:00.000Z');
    assert.equal(iso({ year: 2026, month: 10, day: 4, hour: 2, minute: 15 }, 'Australia/Lord_Howe'), '2026-10-03T15:30:00.000Z');
  });

  it('nextTimeOfDay: DST gap, overlap and weekday filter', () => {
    assert.equal(
      nextTimeOfDay({ tz: 'Europe/London', hour: 1, minute: 30, after: D('2026-03-29T00:00:00Z') }).toISOString(),
      '2026-03-29T01:00:00.000Z',
    );
    const first = nextTimeOfDay({ tz: 'Europe/London', hour: 1, minute: 30, after: D('2026-10-24T12:00:00Z') });
    assert.equal(first.toISOString(), '2026-10-25T00:30:00.000Z');
    assert.equal(
      nextTimeOfDay({ tz: 'Europe/London', hour: 1, minute: 30, after: first }).toISOString(),
      '2026-10-26T01:30:00.000Z',
    );
    assert.equal(
      nextTimeOfDay({ tz: 'UTC', hour: 9, minute: 0, weekdays: [1], after: D('2026-10-11T12:00:00Z') }).toISOString(),
      '2026-10-12T09:00:00.000Z',
    );
    // 09:00 Mon, after Tue → next Monday.
    assert.equal(
      nextTimeOfDay({ tz: 'UTC', hour: 9, minute: 0, weekdays: [1], after: D('2026-10-13T10:00:00Z') }).toISOString(),
      '2026-10-19T09:00:00.000Z',
    );
    // Reminders §8 case 2.2: the DST gap fires once at the first valid instant, then the next day.
    const gap = nextTimeOfDay({ tz: 'Europe/London', hour: 1, minute: 30, after: D('2026-03-28T12:00:00Z') });
    assert.ok(gap >= D('2026-03-29T01:00:00Z') && gap <= D('2026-03-29T01:30:00Z'), gap.toISOString());
    assert.equal(
      nextTimeOfDay({ tz: 'Europe/London', hour: 1, minute: 30, after: gap }).toISOString(),
      '2026-03-30T00:30:00.000Z',
    );
    // Strictly after.
    assert.equal(
      nextTimeOfDay({ tz: 'UTC', hour: 9, minute: 0, after: D('2026-10-12T09:00:00Z') }).toISOString(),
      '2026-10-13T09:00:00.000Z',
    );
  });

  it('parseTimeOfDay and formatTimeOfDay', () => {
    assert.deepEqual(parseTimeOfDay('9:30pm'), { hour: 21, minute: 30 });
    assert.deepEqual(parseTimeOfDay('12am'), { hour: 0, minute: 0 });
    assert.deepEqual(parseTimeOfDay('12pm'), { hour: 12, minute: 0 });
    assert.deepEqual(parseTimeOfDay('9'), { hour: 9, minute: 0 });
    assert.deepEqual(parseTimeOfDay('21:30'), { hour: 21, minute: 30 });
    assert.deepEqual(parseTimeOfDay('noon'), { hour: 12, minute: 0 });
    assert.equal(parseTimeOfDay('24:00'), null);
    assert.equal(parseTimeOfDay('13pm'), null);
    assert.equal(parseTimeOfDay('9:60'), null);
    assert.equal(parseTimeOfDay('abc'), null);
    assert.equal(formatTimeOfDay({ hour: 9, minute: 5 }), '09:05');
  });

  it('parseWeekdays', () => {
    assert.deepEqual(parseWeekdays('fri-mon'), [0, 1, 5, 6]);
    assert.deepEqual(parseWeekdays('weekdays'), [1, 2, 3, 4, 5]);
    assert.deepEqual(parseWeekdays('weekends'), [0, 6]);
    assert.deepEqual(parseWeekdays('all'), [0, 1, 2, 3, 4, 5, 6]);
    assert.deepEqual(parseWeekdays('mon, wed,Fri'), [1, 3, 5]);
    assert.deepEqual(parseWeekdays('0,6'), [0, 6]);
    assert.equal(parseWeekdays('xyz'), null);
    assert.equal(parseWeekdays(''), null);
    assert.deepEqual(WEEKDAY_NAMES, ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat']);
  });

  it('parseBirthday', () => {
    assert.deepEqual(parseBirthday('02-29'), { month: 2, day: 29, year: null });
    assert.equal(parseBirthday('02-30'), null);
    assert.equal(parseBirthday('14/03'), null);
    assert.deepEqual(parseBirthday('1990-03-14'), { month: 3, day: 14, year: 1990 });
    assert.deepEqual(parseBirthday('march 14'), { month: 3, day: 14, year: null });
    assert.deepEqual(parseBirthday('14 mar 1990'), { month: 3, day: 14, year: 1990 });
    assert.deepEqual(parseBirthday('Sept 5'), { month: 9, day: 5, year: null });
    assert.equal(parseBirthday('03-14', { requireYear: true }), null);
    assert.equal(parseBirthday('1800-03-14'), null);
    assert.equal(parseBirthday('2099-03-14', { now: D('2026-10-12T00:00:00Z') }), null);
    assert.equal(parseBirthday('13-01'), null);
    assert.equal(parseBirthday('2023-02-29'), null);
  });

  it('nextIntervalRun without a window', () => {
    // 3.1: every 2h from the anchor.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 120, anchor: D('2026-10-12T00:00:00Z'), after: D('2026-10-12T03:10:00Z') }).toISOString(),
      '2026-10-12T04:00:00.000Z',
    );
    // Strictly after: on the grid, the next grid point.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 120, anchor: D('2026-10-12T00:00:00Z'), after: D('2026-10-12T04:00:00Z') }).toISOString(),
      '2026-10-12T06:00:00.000Z',
    );
    // No anchor: the grid starts at `after`.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 30, after: D('2026-10-12T03:10:00Z') }).toISOString(),
      '2026-10-12T03:40:00.000Z',
    );
    // 3.4: a weekly grid on Mondays never lands on a Tuesday.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 10_080, anchor: D('2026-10-12T00:00:00Z'), weekdays: [2], after: D('2026-10-12T00:00:01Z') }),
      null,
    );
    // Weekday filter skips ahead on a fine grid.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, anchor: D('2026-10-12T00:00:00Z'), weekdays: [3], after: D('2026-10-12T05:30:00Z') }).toISOString(),
      '2026-10-14T00:00:00.000Z',
    );
    assert.equal(nextIntervalRun({ tz: 'UTC', intervalMinutes: 0 }), null);
    assert.equal(nextIntervalRun({ tz: 'UTC', intervalMinutes: 10_081 }), null);
  });

  it('nextIntervalRun respects the window', () => {
    // 3.2: every 3h between 09:00 and 17:00 in Berlin (CEST).
    assert.equal(
      nextIntervalRun({ tz: 'Europe/Berlin', intervalMinutes: 180, windowStart: '09:00', windowEnd: '17:00', after: D('2026-10-12T07:10:00Z') }).toISOString(),
      '2026-10-12T10:00:00.000Z',
    );
    assert.equal(
      nextIntervalRun({ tz: 'Europe/Berlin', intervalMinutes: 180, windowStart: '09:00', windowEnd: '17:00', after: D('2026-10-12T15:30:00Z') }).toISOString(),
      '2026-10-13T07:00:00.000Z',
    );
    // 3.3: a window that crosses midnight.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '22:00', windowEnd: '02:00', after: D('2026-10-12T23:30:00Z') }).toISOString(),
      '2026-10-13T00:00:00.000Z',
    );
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '22:00', windowEnd: '02:00', after: D('2026-10-13T01:30:00Z') }).toISOString(),
      '2026-10-13T22:00:00.000Z',
    );
    // The spec examples: an hourly window 09:00-17:00 UTC.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '09:00', windowEnd: '17:00', after: D('2026-10-12T17:30:00Z') }).toISOString(),
      '2026-10-13T09:00:00.000Z',
    );
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '09:00', windowEnd: '17:00', after: D('2026-10-12T10:10:00Z') }).toISOString(),
      '2026-10-12T11:00:00.000Z',
    );
    // The end is exclusive: the 17:00 slot is not part of the window.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '09:00', windowEnd: '17:00', after: D('2026-10-12T15:30:00Z') }).toISOString(),
      '2026-10-12T16:00:00.000Z',
    );
    // A window belongs to its start day for the weekday filter.
    assert.equal(
      nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: '22:00', windowEnd: '02:00', weekdays: [1], after: D('2026-10-13T00:30:00Z') }).toISOString(),
      '2026-10-13T01:00:00.000Z',
    );
    // An unreadable bound gives null.
    assert.equal(nextIntervalRun({ tz: 'UTC', intervalMinutes: 60, windowStart: 'later', windowEnd: '17:00' }), null);
  });

  it('parseDateInput', () => {
    const now = D('2026-10-12T10:00:00Z');
    assert.equal(parseDateInput('2h', 'UTC', { now }).toISOString(), '2026-10-12T12:00:00.000Z');
    assert.equal(parseDateInput('in 2 days', 'UTC', { now }).toISOString(), '2026-10-14T10:00:00.000Z');
    assert.equal(parseDateInput('2026-12-25 09:30', 'UTC', { now }).toISOString(), '2026-12-25T09:30:00.000Z');
    assert.equal(parseDateInput('2026-12-25', 'Europe/London', { now }).toISOString(), '2026-12-25T00:00:00.000Z');
    assert.equal(parseDateInput('2026-02-30', 'UTC', { now }), null);
    assert.equal(parseDateInput('2026-01-01 09:00', 'UTC', { now }).toISOString(), '2026-01-01T09:00:00.000Z'); // past dates pass through
    assert.equal(parseDateInput('09:00', 'UTC', { now }).toISOString(), '2026-10-13T09:00:00.000Z');
    assert.equal(parseDateInput('11:00', 'UTC', { now }).toISOString(), '2026-10-12T11:00:00.000Z');
    assert.equal(parseDateInput('10-01 08:00', 'UTC', { now }).toISOString(), '2027-10-01T08:00:00.000Z');
    assert.equal(parseDateInput('12-25', 'UTC', { now }).toISOString(), '2026-12-25T00:00:00.000Z');
    assert.equal(parseDateInput('<t:1700000000:R>', 'UTC', { now }).getTime(), 1_700_000_000_000);
    assert.equal(parseDateInput('gibberish', 'UTC', { now }), null);
    assert.equal(parseDateInput('', 'UTC', { now }), null);
  });

  it('discordTimestamp', () => {
    assert.equal(discordTimestamp(new Date(1_700_000_000_000)), '<t:1700000000:f>');
    assert.equal(discordTimestamp(1_700_000_000_999, 'R'), '<t:1700000000:R>');
    assert.equal(discordTimestamp(0, 'x'), '<t:0:f>');
  });
});

// ── customEmbed ──────────────────────────────────────────────────────────────

describe('customEmbed', () => {
  it('validateTemplate reports limits with exact texts', () => {
    const long = validateTemplate({ embed: { title: 'x'.repeat(300) } });
    assert.equal(long.ok, false);
    assert.deepEqual(long.errors, ['Title is 300 characters (max 256).']);

    const fields = Array.from({ length: 26 }, (_, i) => ({ name: `n${i}`, value: 'v' }));
    const many = validateTemplate({ embed: { title: 't', fields } });
    assert.ok(many.errors.includes('Embed has 26 fields (max 25).'));

    assert.ok(validateTemplate({ content: 'x'.repeat(2001) }).errors.includes('Message content is 2001 characters (max 2000).'));
    assert.ok(validateTemplate({ embed: { description: 'x'.repeat(4097) } }).errors.includes('Description is 4097 characters (max 4096).'));
    assert.ok(validateTemplate({ embed: { title: 't', image: 'ftp://x/y.png' } }).errors.includes('Image must be an http(s) URL.'));
    assert.deepEqual(validateTemplate({ embed: { title: 't', image: '{user.avatar}', thumbnail: 'attachment://a.png' } }).errors, []);
    assert.deepEqual(validateTemplate(emptyTemplate()).errors, [EMPTY_MESSAGE_ERROR]);
    assert.equal(EMPTY_MESSAGE_ERROR, 'The message is empty. Add some text or an embed.');
    assert.equal(validateTemplate(emptyTemplate(), { requireSomething: false }).ok, true);
    assert.equal(validateTemplate({ content: 'hi' }).ok, true);
  });

  it('validates the embed total and the button rules', () => {
    const fields = Array.from({ length: 10 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1000) }));
    assert.ok(validateTemplate({ embed: { fields } }).errors.some((e) => e.startsWith('Embed text totals')));

    const longUrl = validateTemplate({ buttons: [{ label: 'x', url: `https://a.example/${'a'.repeat(600)}` }] });
    assert.equal(longUrl.ok, false);
    assert.ok(longUrl.errors.some((e) => e.includes('512')), longUrl.errors.join('|'));

    assert.ok(validateTemplate({ buttons: [{ label: 'x', url: 'javascript:alert(1)' }] }).errors.includes('Button 1 URL must be an http(s) URL.'));
    assert.ok(validateTemplate({ buttons: [{ label: 'x'.repeat(81), url: 'https://a.example/' }] }).errors.some((e) => e.includes('label is 81')));
    assert.ok(validateTemplate({ buttons: [{ label: 'x', url: 'https://a.example/', emoji: 'not an emoji' }] }).errors.some((e) => e.includes('emoji')));
    assert.equal(validateTemplate({ buttons: [{ label: 'x', url: 'https://a.example/{user.id}', emoji: '🎉' }] }).ok, true);
    assert.equal(validateTemplate({ buttons: [{ label: 'x', url: 'https://a.example/', emoji: '<:party:123456789012345678>' }] }).ok, true);
    const tooMany = Array.from({ length: 26 }, () => ({ label: 'x', url: 'https://a.example/' }));
    assert.ok(validateTemplate({ buttons: tooMany }).errors.includes('Too many link buttons (26, max 25).'));
  });

  it('renderTemplate clamps a title that grows past 256 after expansion', () => {
    const out = renderTemplate({ embed: { title: '{1}'.repeat(10) } }, { args: ['x'.repeat(100)] });
    assert.ok(out.embeds[0].data.title.length <= 256);
    assert.ok(out.embeds[0].data.title.length > 200);
  });

  it('renderTemplate keeps the embed total within 6000', () => {
    const fields = Array.from({ length: 10 }, (_, i) => ({ name: `n${i}`, value: 'v'.repeat(1000) }));
    const out = renderTemplate({ embed: { title: 'T', description: 'd'.repeat(4000), fields } }, {});
    const e = out.embeds[0].data;
    const total = (e.title?.length ?? 0) + (e.description?.length ?? 0)
      + (e.fields ?? []).reduce((n, f) => n + f.name.length + f.value.length, 0);
    assert.ok(total <= 6000, `total ${total}`);
    assert.ok(e.fields.length >= 1);
  });

  it('normalizes the legacy welcome shape', () => {
    const t = normalizeTemplate({ title: 'Hi', thumbnail: 'user_avatar', footer: 'f', card: true });
    assert.equal(t.embed.thumbnail, '{user.avatar}');
    assert.equal(t.embed.footer.text, 'f');
    assert.ok(!('card' in t.embed));
    assert.equal(normalizeTemplate({ title: 'Hi', thumbnail: 'server_icon' }).embed.thumbnail, '{server.icon}');
    assert.equal(normalizeTemplate({ title: 'Hi', thumbnail: 'none' }).embed.thumbnail, undefined);
    assert.deepEqual(normalizeTemplate('plain text'), { content: 'plain text', embed: null, buttons: [] });
    assert.deepEqual(normalizeTemplate('   '), emptyTemplate());
    assert.deepEqual(normalizeTemplate(null), emptyTemplate());
    assert.deepEqual(normalizeTemplate([1, 2]), emptyTemplate());
    assert.notEqual(emptyTemplate(), emptyTemplate());
    // A colour-only embed is kept in template form, but it is not visible.
    assert.deepEqual(normalizeTemplate({ embed: { color: 0xff0000 } }).embed, { color: 0xff0000 });
    assert.equal(renderEmbed({ color: 0xff0000 }, {}), null);
    assert.equal(normalizeTemplate({ color: 0xff0000 }).embed, null);
  });

  it('parseColor and formatColor', () => {
    assert.equal(parseColor('#fff'), 0xffffff);
    assert.equal(parseColor('blurple'), 0x5865f2);
    assert.equal(parseColor('nope'), null);
    assert.equal(parseColor('5865F2'), 0x5865f2);
    assert.equal(parseColor('0x5865f2'), 0x5865f2);
    assert.equal(parseColor(' Red '), 0xed4245);
    assert.equal(parseColor(0x123456), 0x123456);
    assert.equal(parseColor(-1), null);
    assert.equal(parseColor(0x1000000), null);
    assert.equal(formatColor(0x5865f2), '#5865F2');
    assert.equal(formatColor('x'), '#000000');
  });

  it('renders nothing for an empty template or an unresolved image', () => {
    assert.equal(renderTemplate(emptyTemplate(), {}), null);
    assert.equal(renderEmbed({ image: '{user.avatar}' }, {}), null);
    assert.equal(renderTemplate({ content: '   ' }, {}), null);
    assert.equal(renderTemplate({ content: 'hi' }, {}).content, 'hi');
    const withText = renderEmbed({ title: 't', image: '{user.avatar}' }, {});
    assert.equal(withText.data.image, undefined);
    const ok = renderEmbed({ title: 't', thumbnail: '{user.avatar}' }, { user: fakeUser() });
    assert.equal(ok.data.thumbnail.url, 'https://cdn/a.png');
    assert.equal(ok.data.color, COLORS.INFO);
    assert.equal(renderEmbed({ title: 't' }, {}, { defaultColor: null }).data.color, undefined);
    assert.equal(renderEmbed({ title: 't', color: 0x123456 }, {}).data.color, 0x123456);
  });

  it('renders link buttons into rows of five and drops bad ones', () => {
    const buttons = Array.from({ length: 7 }, (_, i) => ({ label: `b${i}`, url: `https://a.example/${i}` }));
    const out = renderTemplate({ content: 'x', buttons }, {});
    assert.equal(out.components.length, 2);
    assert.equal(out.components[0].components.length, 5);
    assert.equal(out.components[1].components.length, 2);
    assert.equal(out.components[0].components[0].data.url, 'https://a.example/0');

    const bad = renderTemplate({ content: 'x', buttons: [
      { label: 'long', url: `https://a.example/${'a'.repeat(600)}` },
      { label: 'ftp', url: 'ftp://x' },
      { label: 'unresolved', url: '{user.avatar}' },
      { label: 'good', url: 'https://a.example/ok', emoji: 'not an emoji' },
    ] }, {});
    assert.equal(bad.components.length, 1);
    assert.equal(bad.components[0].components.length, 1);
    assert.equal(bad.components[0].components[0].data.emoji, undefined);

    const emoji = renderTemplate({ content: 'x', buttons: [{ label: 'a', url: 'https://a.example/', emoji: '🎉' }, { label: 'b', url: 'https://a.example/', emoji: '<a:party:123456789012345678>' }] }, {});
    assert.deepEqual(emoji.components[0].components[0].data.emoji, { name: '🎉' });
    assert.deepEqual(emoji.components[0].components[1].data.emoji, { id: '123456789012345678', name: 'party', animated: true });

    // Link rows plus extra components never exceed five rows.
    const extra = Array.from({ length: 4 }, () => ({ type: 1, components: [] }));
    const many = Array.from({ length: 15 }, (_, i) => ({ label: `b${i}`, url: `https://a.example/${i}` }));
    const capped = renderTemplate({ content: 'x', buttons: many }, {}, { extraComponents: extra });
    assert.equal(capped.components.length, 5);
    assert.equal(capped.components[0].components.length, 5);
  });

  it('uses the caller allowedMentions verbatim and always returns embeds and components', () => {
    const out = renderTemplate({ content: 'x' }, {}, { allowedMentions: { parse: [], users: ['1'] } });
    assert.deepEqual(out.allowedMentions, { parse: [], users: ['1'] });
    assert.deepEqual(renderTemplate({ content: 'x' }, {}).allowedMentions, { parse: ['users'] });
    assert.deepEqual(out.embeds, []);
    assert.deepEqual(out.components, []);
  });

  it('round-trips Discord/Discohook JSON through templateFromJson', () => {
    const json = JSON.stringify({ content: 'x', embeds: [{ title: 't', color: 5814783 }] });
    const { template, errors } = templateFromJson(json);
    assert.deepEqual(errors, []);
    assert.equal(template.content, 'x');
    assert.equal(template.embed.title, 't');
    assert.equal(template.embed.color, 5814783);
    const again = templateFromJson(templateToJson(template));
    assert.deepEqual(again.template, template);

    const withButtons = templateFromJson(JSON.stringify({
      content: 'x',
      components: [{ type: 1, components: [{ type: 2, style: 5, label: 'Go', url: 'https://a.example/' }, { type: 2, style: 1, label: 'No', custom_id: 'x' }] }],
    }));
    assert.deepEqual(withButtons.template.buttons, [{ label: 'Go', url: 'https://a.example/', emoji: null }]);

    assert.deepEqual(templateFromJson(5).errors, ['No JSON was provided.']);
    assert.deepEqual(templateFromJson(`"${'x'.repeat(21_000)}"`).errors, ['The JSON is larger than 20 KB.']);
    assert.match(templateFromJson('{nope').errors[0], /^That is not valid JSON:/);
    assert.equal(templateFromJson('{}').template, null);
  });

  it('specFromEmbed accepts builders and API embeds', () => {
    const built = new EmbedBuilder().setTitle('T').setDescription('D').setColor(0x123456).setFooter({ text: 'F', iconURL: 'https://a.example/i.png' })
      .addFields({ name: 'n', value: 'v', inline: true });
    const spec = specFromEmbed(built);
    assert.equal(spec.title, 'T');
    assert.equal(spec.color, 0x123456);
    assert.deepEqual(spec.footer, { text: 'F', iconUrl: 'https://a.example/i.png' });
    assert.deepEqual(spec.fields, [{ name: 'n', value: 'v', inline: true }]);
    assert.equal(specFromEmbed({ title: 'api', image: { url: 'https://a.example/x.png' } }).image, 'https://a.example/x.png');
    assert.deepEqual(specFromEmbed(null), {});
    assert.deepEqual(normalizeLegacyEmbed('nope'), {});
  });

  it('isHttpUrl', () => {
    assert.equal(isHttpUrl('https://a.example/x'), true);
    assert.equal(isHttpUrl('http://a.example'), true);
    assert.equal(isHttpUrl('ftp://a.example'), false);
    assert.equal(isHttpUrl('https://a.example/a b'), false);
    assert.equal(isHttpUrl('notaurl'), false);
    assert.equal(isHttpUrl(5), false);
    assert.equal(isHttpUrl(`https://a.example/${'a'.repeat(2100)}`), false);
  });

  it('buildTemplateModal prefills, clamps and can refuse an overflow', () => {
    const modal = buildTemplateModal({
      customId: 'x', title: 't',
      template: { content: 'hello', embed: { title: 'T', description: 'd'.repeat(4096), color: 0x5865f2, footer: 'F' } },
    });
    const inputs = Object.fromEntries(modal.toJSON().components.map((row) => [row.components[0].custom_id, row.components[0]]));
    assert.deepEqual(Object.keys(inputs), Object.values(TEMPLATE_FIELD_IDS));
    assert.equal(inputs['tpl-description'].value.length, 4000);
    assert.equal(inputs['tpl-description'].max_length, MODAL_INPUT_MAX.description);
    assert.equal(inputs['tpl-color'].value, '#5865F2');
    assert.equal(inputs['tpl-content'].value, 'hello');
    assert.equal(inputs['tpl-title'].required, false);
    assert.equal(buildTemplateModal({ customId: 'x', title: 'y'.repeat(100), template: null }).toJSON().title.length, 45);
    // No prefill for an empty template.
    const empty = buildTemplateModal({ customId: 'x', template: null }).toJSON();
    for (const row of empty.components) assert.equal(row.components[0].value, undefined);

    assert.throws(
      () => buildTemplateModal({ customId: 'x', title: 't', template: { embed: { description: 'd'.repeat(4096) } }, overflow: 'throw' }),
      (err) => err instanceof RangeError && err.fields.includes('description'),
    );
    assert.doesNotThrow(() => buildTemplateModal({ customId: 'x', title: 't', template: { embed: { description: 'd'.repeat(4000) } }, overflow: 'throw' }));
  });

  it('parseTemplateModal merges the submitted values', () => {
    const modalOf = (values) => ({
      fields: {
        getTextInputValue: (id) => {
          if (!(id in values)) throw new Error(`no field ${id}`);
          return values[id];
        },
      },
    });
    const all = { 'tpl-content': 'hi', 'tpl-title': 'Title', 'tpl-description': 'Desc', 'tpl-color': 'blurple', 'tpl-footer': 'Foot' };
    const base = { embed: { title: 'old', author: { name: 'A' }, fields: [{ name: 'n', value: 'v' }], footer: { text: 'x', iconUrl: 'https://a.example/i.png' } } };
    const { template, errors } = parseTemplateModal(modalOf(all), base);
    assert.deepEqual(errors, []);
    assert.equal(template.content, 'hi');
    assert.equal(template.embed.title, 'Title');
    assert.equal(template.embed.color, 0x5865f2);
    assert.equal(template.embed.footer.text, 'Foot');
    assert.equal(template.embed.footer.iconUrl, 'https://a.example/i.png');
    assert.deepEqual(template.embed.author, { name: 'A', iconUrl: null, url: null });
    assert.equal(template.embed.fields.length, 1);
    assert.equal(base.embed.title, 'old'); // the base is not mutated

    // Empty inputs clear their part.
    const cleared = parseTemplateModal(modalOf({ ...all, 'tpl-title': '', 'tpl-footer': '  ', 'tpl-color': '' }), base);
    assert.equal(cleared.template.embed.title, undefined);
    assert.equal(cleared.template.embed.footer, undefined);
    assert.equal(cleared.template.embed.color, undefined);

    // An invalid colour keeps the old colour and reports an error.
    const badColor = parseTemplateModal(modalOf({ ...all, 'tpl-color': 'zzz' }), { embed: { title: 't', color: 0x123456 } });
    assert.ok(badColor.errors.some((e) => e.startsWith('Colour must be a hex value')));
    assert.equal(badColor.template.embed.color, 0x123456);

    // A missing field is an error, never empty input.
    const { 'tpl-footer': _omit, ...withoutFooter } = all;
    const stale = parseTemplateModal(modalOf(withoutFooter), { embed: { title: 't', footer: { text: 'keep' } } });
    assert.ok(stale.errors.length > 0);
    assert.ok(stale.errors.includes('This form is out of date. Open it again.'));
    assert.equal(stale.template.embed.footer.text, 'keep');

    // A long description that was only prefilled (cut to 4000) is kept whole.
    const longBase = { embed: { description: 'd'.repeat(4096) } };
    const kept = parseTemplateModal(modalOf({ ...all, 'tpl-description': 'd'.repeat(4000) }), longBase);
    assert.equal(kept.template.embed.description.length, 4096);
    const edited = parseTemplateModal(modalOf({ ...all, 'tpl-description': 'short' }), longBase);
    assert.equal(edited.template.embed.description, 'short');

    // Over-limit content is reported, and the template is still returned.
    const tooLong = parseTemplateModal(modalOf({ ...all, 'tpl-content': 'x'.repeat(2500) }));
    assert.ok(tooLong.errors.some((e) => e.startsWith('Message content is 2500')));
    assert.ok(tooLong.template);
  });

  it('parseTemplateModal reports an emptied form as EMPTY_MESSAGE_ERROR unless requireSomething is false', () => {
    const modalOf = (values) => ({ fields: { getTextInputValue: (id) => values[id] } });
    const blank = { 'tpl-content': '', 'tpl-title': ' ', 'tpl-description': '', 'tpl-color': '', 'tpl-footer': '' };
    const base = { content: 'old', embed: { title: 'old' } };

    assert.deepEqual(parseTemplateModal(modalOf(blank), base).errors, [EMPTY_MESSAGE_ERROR]); // the default
    assert.deepEqual(parseTemplateModal(modalOf(blank), base, { requireSomething: true }).errors, [EMPTY_MESSAGE_ERROR]);
    const allowed = parseTemplateModal(modalOf(blank), base, { requireSomething: false });
    assert.deepEqual(allowed.errors, [], 'an emptied form is allowed when the caller has a default to fall back on');
    assert.equal(allowed.template.content, null);
    assert.equal(allowed.template.embed, null);

    // Only the empty check is lifted: real problems and stale forms still report.
    const bad = parseTemplateModal(modalOf({ ...blank, 'tpl-color': 'zzz' }), base, { requireSomething: false });
    assert.equal(bad.errors.length, 1);
    assert.match(bad.errors[0], /^Colour must be a hex value/);
    const stale = parseTemplateModal(modalOf({ 'tpl-content': '' }), base, { requireSomething: false });
    assert.ok(stale.errors.includes('This form is out of date. Open it again.'));
  });
});

// ── safeRegex ────────────────────────────────────────────────────────────────

describe('safeRegex', () => {
  it('rejects dangerous or disallowed patterns', () => {
    for (const bad of ['(a+)+', '(a|aa)*', '(\\w+\\s?)*', '(.*a){5}', '((a+))+', '(a{2})+', '\\1', '(a)\\k<x>']) {
      assert.equal(validateRegex(bad).ok, false, bad);
    }
    assert.equal(validateRegex('abc', 'g').ok, false);
    assert.equal(validateRegex('abc', 'y').ok, false);
    assert.equal(validateRegex('abc', 'v').ok, false);
    assert.equal(validateRegex('a'.repeat(201)).ok, false);
    assert.equal(validateRegex('').ok, false);
    assert.equal(validateRegex(5).ok, false);
    assert.equal(validateRegex('(').ok, false);
    assert.equal(validateRegex('a'.repeat(201)).reason, 'Pattern is 201 characters (max 200).');
    assert.equal(validateRegex('abc', 'g').reason, 'Flag "g" is not allowed (use i, m, s or u).');
    assert.match(validateRegex('(a+)+').reason, /^Nested repetition/);
    assert.match(validateRegex('\\1').reason, /^Backreferences/);
  });

  it('accepts safe patterns', () => {
    for (const good of ['^hello\\s+world$', '[a-z]+@[a-z]+\\.com', '(foo|bar)baz', '(ab)+', '(a|b)?', '[(]+', '\\\\1', 'a+?b', '(?:ab)+c', '(?<n>ab)+']) {
      assert.deepEqual(validateRegex(good), { ok: true }, good);
    }
    assert.deepEqual(validateRegex('abc', 'iimm'), { ok: true });
  });

  it('compileSafeRegex validates, compiles and caches', () => {
    const a = compileSafeRegex('^hello$');
    assert.ok(a instanceof RegExp);
    assert.equal(a.flags, 'i');
    assert.equal(compileSafeRegex('^hello$'), a);
    assert.equal(compileSafeRegex('(a+)+'), null);
    assert.equal(compileSafeRegex('x', 'g'), null);
  });

  it('safeRegexTest matches normally and survives catastrophic backtracking', () => {
    assert.equal(safeRegexTest(/^hello/i, 'Hello there'), true);
    assert.equal(safeRegexTest(/^hello/i, 'nope'), false);
    assert.equal(safeRegexTest('not a regex', 'x'), false);
    assert.equal(safeRegexTest(/a/g, 'aaa'), true);
    const re = new RegExp('(a+)+$');
    const t0 = Date.now();
    assert.equal(safeRegexTest(re, `${'a'.repeat(40)}!`), false);
    assert.ok(Date.now() - t0 < 500, 'timed out quickly');
    assert.equal(isQuarantined(re), true);
    assert.equal(safeRegexTest(re, 'aaa'), false); // quarantined: skipped
    assert.equal(isQuarantined(/(a+)+$/), true); // keyed by source and flags
    assert.equal(isQuarantined(/never-used-regex/), false);
    assert.equal(isQuarantined('x'), false);
  });

  it('safeRegexFirstMatch returns the first index and releases innocent regexes', () => {
    assert.equal(safeRegexFirstMatch([/x/i, /hello/i, /hel/i], 'say hello'), 1);
    assert.equal(safeRegexFirstMatch([/x/i], 'nothing here'), -1);
    assert.equal(safeRegexFirstMatch([], 'x'), -1);
    assert.equal(safeRegexFirstMatch('nope', 'x'), -1);
    const innocent = /x/i;
    const slow = new RegExp('(b+)+$');
    const t0 = Date.now();
    assert.equal(safeRegexFirstMatch([innocent, slow, /hello/i], `${'b'.repeat(40)}! hello`), 2);
    assert.ok(Date.now() - t0 < 1000);
    assert.equal(isQuarantined(innocent), false);
    assert.equal(isQuarantined(slow), true);
  });

  it('escapeRegex output compiles under the u flag', () => {
    const escaped = escapeRegex('foo-bar/baz');
    assert.doesNotThrow(() => new RegExp(escaped, 'iu'));
    assert.equal(escaped, 'foo-bar/baz');
    assert.equal(escapeRegex('a.b*c'), 'a\\.b\\*c');
    for (const ch of '.*+?^${}()|[]\\') {
      const re = new RegExp(escapeRegex(ch), 'u');
      assert.equal(re.test(ch), true, ch);
    }
    assert.equal(escapeRegex(null), '');
  });

  it('containsWord matches whole words only', () => {
    assert.equal(containsWord('Scunthorpe', 'cunt'), false);
    assert.equal(containsWord('you are a jerk!', 'jerk'), true);
    assert.equal(containsWord('a foo-bar!', 'foo-bar'), true);
    assert.equal(containsWord('JERK', 'jerk'), true);
    assert.equal(containsWord('JERK', 'jerk', { caseSensitive: true }), false);
    assert.equal(containsWord('x', ''), false);
    assert.equal(containsWord('café au lait', 'caf'), false);
    assert.equal(containsWord('a (b) c', '(b)'), true);
  });
});

// ── safeFetch ────────────────────────────────────────────────────────────────

describe('safeFetch addresses', () => {
  it('blocks private, reserved and embedded-private addresses', () => {
    for (const ip of [
      '127.0.0.1', '10.1.2.3', '100.64.0.1', '169.254.169.254', '0.0.0.0', '172.16.0.1', '192.168.1.1', '224.0.0.1',
      '255.255.255.255', '::1', 'fd00::1', 'fe80::1', '::ffff:127.0.0.1', '64:ff9b::7f00:1', '2002:7f00:1::',
      '::7f00:1', '64:ff9b:1::a00:1', '::', 'not-an-ip', '', '[::1]', 'fe80::1%eth0',
    ]) {
      assert.equal(isBlockedAddress(ip), true, ip);
    }
    assert.equal(isBlockedAddress(null), true);
  });

  it('allows public addresses', () => {
    for (const ip of ['1.1.1.1', '8.8.8.8', '2606:4700:4700::1111', '64:ff9b::101:101', '93.184.216.34', '2002:101:101::']) {
      assert.equal(isBlockedAddress(ip), false, ip);
    }
  });

  it('assertPublicUrl rejects bad URLs before any DNS lookup', async () => {
    const reject = (url, code) => assert.rejects(assertPublicUrl(url), (err) => err instanceof SafeFetchError && err.code === code, `${url} → ${code}`);
    await reject('ftp://x', 'INVALID_URL');
    await reject('not a url', 'INVALID_URL');
    await reject('http://127.0.0.1/', 'BLOCKED_ADDRESS');
    await reject('http://2130706433/', 'BLOCKED_ADDRESS');
    await reject('http://0x7f.1/', 'BLOCKED_ADDRESS');
    await reject('http://[::1]/', 'BLOCKED_ADDRESS');
    await reject('http://169.254.169.254/latest/meta-data', 'BLOCKED_ADDRESS');
    await reject('http://user:pw@example.com', 'INVALID_URL');
    await reject('http://:pw@example.com', 'INVALID_URL');
  });

  it('SafeFetchError carries code, status, headers and retryAfterMs', () => {
    const e = new SafeFetchError('HTTP_ERROR', 404);
    assert.equal(e.status, 404);
    assert.equal(e.code, 'HTTP_ERROR');
    assert.equal(e.message, 'HTTP 404');
    assert.equal(e.name, 'SafeFetchError');
    const e2 = new SafeFetchError('HTTP_ERROR', 'HTTP 429', { status: 429, retryAfterMs: 5000, headers: { 'retry-after': '5' } });
    assert.equal(e2.retryAfterMs, 5000);
    assert.deepEqual(e2.headers, { 'retry-after': '5' });
    assert.equal(new SafeFetchError('TIMEOUT').message, 'TIMEOUT');
    assert.match(DEFAULT_USER_AGENT, /^DiscordBot\/1\.0 \(\+self-hosted; contact: .+\)$/);
  });
});

describe('safeFetch requests', () => {
  afterEach(() => mock.restoreAll());

  // A stand-in for http.request: `respond(options, req)` decides what the "server" does.
  function stubNetwork({ hosts = {}, respond }) {
    const requests = [];
    mock.method(dns.promises, 'lookup', async (host) => {
      const found = hosts[host];
      if (!found) throw Object.assign(new Error('ENOTFOUND'), { code: 'ENOTFOUND' });
      return found.map((address) => ({ address, family: address.includes(':') ? 6 : 4 }));
    });
    mock.method(http, 'request', (options) => {
      const req = new EventEmitter();
      req.destroy = () => {};
      req.end = () => {
        requests.push({ ...options, headers: { ...options.headers } });
        process.nextTick(() => respond(options, req, requests.length));
      };
      options.signal?.addEventListener('abort', () => req.emit('error', Object.assign(new Error('aborted'), { name: 'AbortError' })));
      return req;
    });
    return requests;
  }

  function response({ status = 200, headers = {}, body = '', complete = true } = {}) {
    const res = new PassThrough();
    res.statusCode = status;
    res.statusMessage = 'Status';
    res.headers = headers;
    res.complete = complete;
    res.end(body);
    return res;
  }

  const PUBLIC = ['93.184.216.34'];

  it('pins the validated addresses into the socket lookup and re-checks them', async () => {
    const requests = stubNetwork({
      hosts: { 'api.test': ['10.0.0.5', '93.184.216.34'].slice(1).concat(['2606:4700::1']) },
      respond: (_o, req) => req.emit('response', response({ headers: { 'content-type': 'application/json' }, body: '{"a":1}' })),
    });
    const data = await safeFetchJson('http://api.test/x?y=1');
    assert.deepEqual(data, { a: 1 });
    const [opts] = requests;
    assert.equal(opts.host, 'api.test');
    assert.equal(opts.path, '/x?y=1');
    assert.equal(opts.agent, false);
    assert.equal(opts.headers['user-agent'], DEFAULT_USER_AGENT);
    assert.equal(opts.headers.accept, 'application/json');
    assert.equal(opts.headers['accept-encoding'], 'gzip, deflate, br');

    // all: true gets every validated address, IPv4 first; the single form gets the first.
    const all = await new Promise((resolve, reject) => opts.lookup('api.test', { all: true }, (e, a) => (e ? reject(e) : resolve(a))));
    assert.deepEqual(all, [{ address: '93.184.216.34', family: 4 }, { address: '2606:4700::1', family: 6 }]);
    const single = await new Promise((resolve, reject) => opts.lookup('api.test', {}, (e, a, f) => (e ? reject(e) : resolve([a, f]))));
    assert.deepEqual(single, ['93.184.216.34', 4]);
    const v6 = await new Promise((resolve, reject) => opts.lookup('api.test', { all: true, family: 6 }, (e, a) => (e ? reject(e) : resolve(a))));
    assert.deepEqual(v6, [{ address: '2606:4700::1', family: 6 }]);
  });

  it('refuses a hostname that resolves to a private address, without connecting', async () => {
    const requests = stubNetwork({
      hosts: { 'evil.test': ['10.0.0.5'], 'mixed.test': ['93.184.216.34', '169.254.169.254'] },
      respond: (_o, req) => req.emit('response', response()),
    });
    for (const host of ['evil.test', 'mixed.test']) {
      await assert.rejects(safeFetch(`http://${host}/`), (err) => err instanceof SafeFetchError && err.code === 'BLOCKED_ADDRESS');
    }
    await assert.rejects(safeFetch('http://missing.test/'), (err) => err.code === 'DNS_FAILED');
    assert.equal(requests.length, 0);
  });

  it('re-checks every redirect hop', async () => {
    const hops = {
      'a.test': 'http://127.0.0.1/admin',
      'b.test': 'http://internal.test/x',
      'c.test': 'http://[::1]:8080/',
      'd.test': 'ftp://other.test/file',
      'e.test': 'http://169.254.169.254/latest/meta-data/',
    };
    const requests = stubNetwork({
      hosts: { ...Object.fromEntries(Object.keys(hops).map((h) => [h, PUBLIC])), 'internal.test': ['192.168.1.10'] },
      respond: (o, req) => req.emit('response', response({ status: 302, headers: { location: hops[o.host] } })),
    });
    const expected = { 'a.test': 'BLOCKED_ADDRESS', 'b.test': 'BLOCKED_ADDRESS', 'c.test': 'BLOCKED_ADDRESS', 'd.test': 'INVALID_URL', 'e.test': 'BLOCKED_ADDRESS' };
    for (const [host, code] of Object.entries(expected)) {
      await assert.rejects(safeFetch(`http://${host}/`), (err) => err instanceof SafeFetchError && err.code === code, host);
    }
    assert.equal(requests.length, 5, 'only the first hop of each chain was requested');
  });

  it('follows public redirects, resets the method and drops credentials across origins', async () => {
    const requests = stubNetwork({
      hosts: { 'a.test': PUBLIC, 'b.test': ['93.184.216.35'] },
      respond: (o, req, n) => {
        if (n === 1) req.emit('response', response({ status: 302, headers: { location: 'http://b.test/final?x=1' } }));
        else req.emit('response', response({ headers: { 'content-type': 'text/plain; charset=utf-8' }, body: 'done' }));
      },
    });
    const res = await safeFetch('http://a.test/start', {
      method: 'POST', body: { hello: 'world' }, headers: { Authorization: 'Bearer secret', Cookie: 'a=b', Host: 'ignored' },
    });
    assert.equal(res.text(), 'done');
    assert.equal(res.url, 'http://b.test/final?x=1');
    assert.equal(res.ok, true);
    assert.equal(requests.length, 2);
    assert.equal(requests[0].method, 'POST');
    assert.equal(requests[0].headers['content-type'], 'application/json');
    assert.equal(requests[0].headers.authorization, 'Bearer secret');
    assert.equal(requests[0].headers.host, undefined);
    assert.equal(requests[1].method, 'GET');
    assert.equal(requests[1].path, '/final?x=1');
    assert.equal(requests[1].headers.authorization, undefined);
    assert.equal(requests[1].headers.cookie, undefined);
    assert.equal(requests[1].headers['content-type'], undefined);
    assert.equal(requests[1].headers['content-length'], undefined);
  });

  it('throws TOO_MANY_REDIRECTS', async () => {
    stubNetwork({
      hosts: { 'loop.test': PUBLIC },
      respond: (_o, req) => req.emit('response', response({ status: 301, headers: { location: '/again' } })),
    });
    await assert.rejects(safeFetch('http://loop.test/', { maxRedirects: 2 }), (err) => err.code === 'TOO_MANY_REDIRECTS');
  });

  it('caps the response size, including after decompression', async () => {
    stubNetwork({
      hosts: { 'big.test': PUBLIC },
      respond: (o, req) => {
        const path = o.path;
        if (path === '/declared') req.emit('response', response({ headers: { 'content-length': '5000' }, body: 'x' }));
        else if (path === '/stream') req.emit('response', response({ body: 'x'.repeat(5000) }));
        else if (path === '/bomb') req.emit('response', response({ headers: { 'content-encoding': 'gzip' }, body: zlib.gzipSync(Buffer.alloc(5_000_000)) }));
        else if (path === '/gzip') req.emit('response', response({ headers: { 'content-encoding': 'gzip', 'content-type': 'text/plain' }, body: zlib.gzipSync('hello gzip') }));
        else req.emit('response', response({ body: 'small' }));
      },
    });
    for (const path of ['/declared', '/stream', '/bomb']) {
      await assert.rejects(safeFetch(`http://big.test${path}`, { maxBytes: 1000 }), (err) => err.code === 'TOO_LARGE', path);
    }
    assert.equal((await safeFetch('http://big.test/gzip', { maxBytes: 1000 })).text(), 'hello gzip');
    assert.equal((await safeFetch('http://big.test/ok', { maxBytes: 1000 })).text(), 'small');
  });

  it('times out when the server never answers', async () => {
    stubNetwork({ hosts: { 'slow.test': PUBLIC }, respond: () => {} });
    await assert.rejects(safeFetch('http://slow.test/', { timeoutMs: 50 }), (err) => err.code === 'TIMEOUT');
  });

  it('times out a stalled body', async () => {
    stubNetwork({
      hosts: { 'stall.test': PUBLIC },
      respond: (_o, req) => {
        const res = new PassThrough();
        res.statusCode = 200; res.headers = {}; res.complete = false;
        req.emit('response', res);
        res.write('partial');
      },
    });
    await assert.rejects(safeFetch('http://stall.test/', { timeoutMs: 80 }), (err) => err.code === 'TIMEOUT');
  });

  it('maps socket errors to NETWORK', async () => {
    stubNetwork({
      hosts: { 'down.test': PUBLIC },
      respond: (_o, req) => req.emit('error', Object.assign(new Error('refused'), { code: 'ECONNREFUSED' })),
    });
    await assert.rejects(safeFetch('http://down.test/'), (err) => err instanceof SafeFetchError && err.code === 'NETWORK' && /ECONNREFUSED/.test(err.message));
  });

  it('wrappers turn HTTP failures into SafeFetchError with status, headers and retry-after', async () => {
    stubNetwork({
      hosts: { 'api.test': PUBLIC },
      respond: (o, req) => {
        if (o.path === '/limited') req.emit('response', response({ status: 429, headers: { 'retry-after': '3', 'x-ratelimit-remaining': '0' }, body: '{}' }));
        else if (o.path === '/notfound') req.emit('response', response({ status: 404, body: 'no' }));
        else if (o.path === '/badjson') req.emit('response', response({ body: '<html>' }));
        else if (o.path === '/image') req.emit('response', response({ headers: { 'content-type': 'image/png' }, body: Buffer.from('not really a png') }));
        else if (o.path === '/svg') req.emit('response', response({ headers: { 'content-type': 'image/svg+xml' }, body: '<svg/>' }));
        else if (o.path === '/text') req.emit('response', response({ headers: { 'content-type': 'text/html' }, body: 'hi' }));
        else if (o.path === '/notmodified') req.emit('response', response({ status: 304 }));
        else {
          const png = Buffer.alloc(24);
          Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(png);
          png.writeUInt32BE(13, 8); png.write('IHDR', 12, 'latin1');
          png.writeUInt32BE(o.path === '/bomb' ? 30000 : 64, 16); png.writeUInt32BE(o.path === '/bomb' ? 30000 : 64, 20);
          req.emit('response', response({ headers: { 'content-type': 'image/png' }, body: png }));
        }
      },
    });
    await assert.rejects(safeFetchJson('http://api.test/limited'), (err) => (
      err.code === 'HTTP_ERROR' && err.status === 429 && err.retryAfterMs === 3000 && err.headers['x-ratelimit-remaining'] === '0'
    ));
    await assert.rejects(safeFetchText('http://api.test/notfound'), (err) => err.code === 'HTTP_ERROR' && err.status === 404);
    await assert.rejects(safeFetchJson('http://api.test/badjson'), (err) => err.code === 'HTTP_ERROR' && /not valid JSON/.test(err.message));
    assert.equal(await safeFetchText('http://api.test/text'), 'hi');

    const conditional = await safeFetch('http://api.test/notmodified', { headers: { 'If-None-Match': 'abc' } });
    assert.equal(conditional.status, 304);
    assert.equal(conditional.ok, false);

    await assert.rejects(safeFetchImage('http://api.test/svg'), (err) => err.code === 'BAD_CONTENT_TYPE');
    await assert.rejects(safeFetchImage('http://api.test/text'), (err) => err.code === 'BAD_CONTENT_TYPE');
    await assert.rejects(safeFetchImage('http://api.test/image'), (err) => err.code === 'BAD_IMAGE');
    await assert.rejects(safeFetchImage('http://api.test/bomb'), (err) => err.code === 'BAD_IMAGE');
    const png = await safeFetchImage('http://api.test/small');
    assert.equal(readImageSize(png).width, 64);
  });

  it('never connects to a loopback server, by address or by name', async () => {
    mock.restoreAll();
    let hits = 0;
    const server = http.createServer((_req, res) => { hits++; res.end('secret'); });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    const { port } = server.address();
    try {
      for (const url of [`http://127.0.0.1:${port}/`, `http://localhost:${port}/`, `http://[::1]:${port}/`]) {
        await assert.rejects(safeFetch(url), (err) => err instanceof SafeFetchError && err.code === 'BLOCKED_ADDRESS', url);
      }
      assert.equal(hits, 0);
    } finally {
      await new Promise((resolve) => server.close(resolve));
    }
  });
});

// ── interactionHelpers ───────────────────────────────────────────────────────

describe('interactionHelpers', () => {
  const bitsOf = (...keys) => new PermissionsBitField(keys.map((k) => PermissionFlagsBits[k]));

  it('isInteractionGone', () => {
    for (const code of [10062, 40060, 10015, 50027, 'InteractionAlreadyReplied']) assert.equal(isInteractionGone({ code }), true, String(code));
    assert.equal(isInteractionGone({ code: 50013 }), false);
    assert.equal(isInteractionGone(null), false);
  });

  it('permissionNames', () => {
    assert.deepEqual(permissionNames(['ManageRoles', 'EmbedLinks']), ['Manage Roles', 'Embed Links']);
    assert.deepEqual(permissionNames('ManageGuild'), ['Manage Server']);
    assert.deepEqual(permissionNames(['ModerateMembers', 'MentionEveryone', 'UseVAD']), ['Timeout Members', 'Mention @everyone', 'Use Voice Activity']);
    assert.deepEqual(permissionNames(PermissionFlagsBits.ManageRoles | PermissionFlagsBits.ManageGuild), ['Manage Server', 'Manage Roles']);
    assert.deepEqual(permissionNames(bitsOf('SendMessages')), ['Send Messages']);
    assert.deepEqual(permissionNames(['ManageRoles', 'ManageRoles', 'Nonsense']), ['Manage Roles']);
    assert.deepEqual(permissionNames(null), []);
  });

  it('missingBotPermissions for channels and guilds', () => {
    const channel = { permissionsFor: () => bitsOf('ViewChannel', 'SendMessages'), guild: { members: { me: { id: 'me' } } } };
    assert.deepEqual(missingBotPermissions(channel, ['ViewChannel', 'EmbedLinks', 'AttachFiles']), ['EmbedLinks', 'AttachFiles']);
    assert.deepEqual(missingBotPermissions(channel, 'ViewChannel'), []);
    const guild = { members: { me: { permissions: bitsOf('ManageRoles') } } };
    assert.deepEqual(missingBotPermissions(guild, ['ManageRoles', 'BanMembers']), ['BanMembers']);
    const admin = { members: { me: { permissions: bitsOf('Administrator') } } };
    assert.deepEqual(missingBotPermissions(admin, ['BanMembers']), []);
    assert.deepEqual(missingBotPermissions({ members: {} }, ['BanMembers']), ['BanMembers']);
    assert.deepEqual(missingBotPermissions(channel, ['NotAPermission']), []);
  });

  it('requireBotPermissions replies when something is missing', async () => {
    const replies = [];
    const interaction = {
      guild: { members: { me: { permissions: bitsOf('ManageRoles') } } },
      channel: { id: '55', permissionsFor: () => bitsOf('ViewChannel'), guild: { members: { me: {} } } },
      deferred: false, replied: false,
      reply: async (p) => { replies.push(p); return { resource: { message: { id: 'm' } } }; },
    };
    assert.equal(await requireBotPermissions(interaction, ['ViewChannel']), true);
    assert.equal(await requireBotPermissions(interaction, ['ViewChannel', 'EmbedLinks']), false);
    assert.equal(replies.length, 1);
    assert.equal(replies[0].ephemeral, true);
    assert.match(replies[0].embeds[0].data.description, /I need \*\*Embed Links\*\* in <#55> to do that\./);
    assert.equal(await requireBotPermissions({ guild: null }, ['BanMembers']), true);
  });

  it('checkChannelUsable', () => {
    assert.equal(checkChannelUsable(null).reason, 'Channel not found');
    assert.equal(checkChannelUsable({ isTextBased: () => false }).reason, 'That is not a text channel');
    const mk = (keys, thread = false) => ({
      id: '7', isTextBased: () => true, isThread: () => thread,
      permissionsFor: () => bitsOf(...keys), guild: { members: { me: {} } },
    });
    assert.deepEqual(checkChannelUsable(mk(['ViewChannel', 'SendMessages', 'EmbedLinks'])), { ok: true, missing: [] });
    const missing = checkChannelUsable(mk(['ViewChannel']));
    assert.equal(missing.ok, false);
    assert.deepEqual(missing.missing, ['SendMessages', 'EmbedLinks']);
    assert.equal(missing.reason, 'I need **Send Messages, Embed Links** in <#7>.');
    // Threads need SendMessagesInThreads instead of SendMessages.
    assert.equal(checkChannelUsable(mk(['ViewChannel', 'SendMessages', 'EmbedLinks'], true)).ok, false);
    assert.equal(checkChannelUsable(mk(['ViewChannel', 'SendMessagesInThreads', 'EmbedLinks'], true)).ok, true);
    assert.deepEqual(checkChannelUsable(mk(['ViewChannel']), { needs: ['ViewChannel'] }), { ok: true, missing: [] });
  });

  describe('checkAssignableRole', () => {
    const makeMe = (keys = ['ManageRoles'], position = 10) => ({ permissions: bitsOf(...keys), roles: { highest: { position } } });
    const makeGuild = (me = makeMe()) => ({ id: 'G', ownerId: 'OWNER', members: { me } });
    const makeRole = (over = {}) => ({ id: 'R', managed: false, position: 5, permissions: new PermissionsBitField(0n), ...over });
    const executor = (position, id = 'U') => ({ id, roles: { highest: { position } } });

    it('applies the rules in order, one failure at a time', () => {
      const guild = makeGuild();
      assert.deepEqual(checkAssignableRole(guild, null), { ok: false, reason: 'Role not found.', dangerous: [] });
      assert.equal(checkAssignableRole(guild, makeRole({ id: 'G' })).reason, '@everyone cannot be assigned.');
      assert.equal(checkAssignableRole(guild, makeRole({ managed: true })).reason, 'That role is managed by an integration.');
      assert.equal(checkAssignableRole(makeGuild(makeMe([])), makeRole()).reason, 'I need the Manage Roles permission.');
      assert.equal(checkAssignableRole(guild, makeRole({ position: 10 })).reason, 'That role is above my highest role.');
      assert.equal(checkAssignableRole(guild, makeRole({ position: 5 }), { executor: executor(3) }).reason, 'That role is above your highest role.');
      assert.equal(checkAssignableRole(guild, makeRole({ position: 5 }), { executor: executor(3, 'OWNER') }).ok, true);
      assert.deepEqual(checkAssignableRole(guild, makeRole(), { executor: executor(8) }), { ok: true, dangerous: [] });
      assert.deepEqual(checkAssignableRole(guild, makeRole()), { ok: true, dangerous: [] });
    });

    it('checks the earlier rule first when several would fail', () => {
      const guild = makeGuild(makeMe([]));
      assert.equal(checkAssignableRole(guild, makeRole({ managed: true, position: 99 })).reason, 'That role is managed by an integration.');
      assert.equal(checkAssignableRole(guild, makeRole({ position: 99 })).reason, 'I need the Manage Roles permission.');
    });

    it('lists dangerous permission keys', () => {
      const guild = makeGuild();
      const role = makeRole({ permissions: bitsOf('ManageGuild') });
      const refused = checkAssignableRole(guild, role);
      assert.equal(refused.ok, false);
      assert.deepEqual(refused.dangerous, ['ManageGuild']);
      assert.match(refused.reason, /Manage Server/);
      const allowed = checkAssignableRole(guild, role, { allowDangerous: true });
      assert.equal(allowed.ok, true);
      assert.deepEqual(allowed.dangerous, ['ManageGuild']);
      const admin = checkAssignableRole(guild, makeRole({ permissions: bitsOf('Administrator') }));
      assert.deepEqual(admin.dangerous, ['Administrator']);
      const several = checkAssignableRole(guild, makeRole({ permissions: bitsOf('BanMembers', 'Administrator', 'ManageMessages') }));
      assert.deepEqual(several.dangerous, ['Administrator', 'BanMembers', 'ManageMessages']);
      assert.equal(checkAssignableRole(guild, makeRole({ permissions: bitsOf('SendMessages') })).ok, true);
    });
  });

  it('pingableRoleIds', () => {
    const roles = new Map([
      ['1', { id: '1', mentionable: true }],
      ['2', { id: '2', mentionable: false }],
      ['G', { id: 'G', mentionable: true }],
    ]);
    const channelWith = (bits) => ({ guild: { id: 'G', roles: { cache: roles } }, permissionsFor: () => bits });
    const without = pingableRoleIds(channelWith(bitsOf('ViewChannel')), { id: 'u' }, ['1', '2', 'G', '999']);
    assert.deepEqual(without, { allowed: ['1'], refused: ['2', 'G', '999'] });
    const withAll = pingableRoleIds(channelWith(bitsOf('MentionEveryone')), { id: 'u' }, ['1', '2', 'G', '999']);
    assert.deepEqual(withAll, { allowed: ['1', '2'], refused: ['G', '999'] });
    assert.deepEqual(pingableRoleIds(channelWith(null), { id: 'u' }, ['2']), { allowed: [], refused: ['2'] });
    assert.deepEqual(pingableRoleIds(channelWith(bitsOf('MentionEveryone')), null, ['2']), { allowed: [], refused: ['2'] });
    assert.deepEqual(pingableRoleIds(null, null, ['1']), { allowed: [], refused: ['1'] });
    assert.deepEqual(pingableRoleIds(channelWith(null), null, []), { allowed: [], refused: [] });
  });

  it('respondAutocomplete clamps to 25 choices and 100 characters', async () => {
    let recorded = null;
    const interaction = { responded: false, respond: async (list) => { recorded = list; } };
    await respondAutocomplete(interaction, Array.from({ length: 30 }, (_, i) => ({ name: `${'n'.repeat(150)}${i}`, value: `${'v'.repeat(150)}${i}` })));
    assert.equal(recorded.length, 25);
    for (const c of recorded) {
      assert.ok(c.name.length <= 100);
      assert.ok(c.value.length <= 100);
    }
    await respondAutocomplete(interaction, [{ value: 'only-value' }, { name: 'n', value: 5 }, { name: 'skip', value: '' }]);
    assert.deepEqual(recorded, [{ name: 'only-value', value: 'only-value' }, { name: 'n', value: 5 }]);
    await respondAutocomplete(interaction, 'nope');
    assert.deepEqual(recorded, []);
    // Errors are swallowed; an answered interaction is left alone.
    await respondAutocomplete({ responded: false, respond: async () => { throw Object.assign(new Error('x'), { code: 10062 }); } }, [{ name: 'a', value: 'a' }]);
    let called = false;
    await respondAutocomplete({ responded: true, respond: async () => { called = true; } }, []);
    assert.equal(called, false);
  });

  describe('safeDefer and safeReply', () => {
    const base = (over = {}) => {
      const calls = [];
      const i = {
        deferred: false, replied: false, ephemeral: null, calls,
        deferReply: async (p) => { calls.push(['deferReply', p]); i.deferred = true; i.ephemeral = Boolean(p?.ephemeral); },
        reply: async (p) => { calls.push(['reply', p]); i.replied = true; return { resource: { message: { id: 'sent' } } }; },
        editReply: async (p) => { calls.push(['editReply', p]); return { id: 'edited' }; },
        followUp: async (p) => { calls.push(['followUp', p]); return { id: 'followed' }; },
        ...over,
      };
      return i;
    };

    it('safeDefer defers once and reports failures', async () => {
      const i = base();
      assert.equal(await safeDefer(i, { ephemeral: true }), true);
      assert.equal(await safeDefer(i), true);
      assert.equal(i.calls.filter(([name]) => name === 'deferReply').length, 1);
      assert.equal(await safeDefer(base({ deferReply: async () => { throw Object.assign(new Error('x'), { code: 10062 }); } })), false);
    });

    it('replies when not acknowledged, and never sends fetchReply or withResponse from the caller', async () => {
      const i = base();
      const msg = await safeReply(i, { content: 'hi', fetchReply: true, withResponse: false });
      assert.deepEqual(msg, { id: 'sent' });
      assert.deepEqual(i.calls[0], ['reply', { content: 'hi', withResponse: true }]);
      assert.deepEqual((await safeReply(base(), 'plain')), { id: 'sent' });
    });

    it('edits after deferReply (without ephemeral) and falls back to followUp when the original was deleted', async () => {
      const i = base();
      await safeDefer(i, { ephemeral: true });
      const out = await safeReply(i, { content: 'x', ephemeral: true });
      assert.deepEqual(out, { id: 'edited' });
      assert.deepEqual(i.calls.at(-1), ['editReply', { content: 'x' }]);

      const deleted = base({ editReply: async () => { throw Object.assign(new Error('Unknown Message'), { code: 10008 }); } });
      await safeDefer(deleted);
      assert.deepEqual(await safeReply(deleted, { content: 'y' }), { id: 'followed' });
      assert.deepEqual(deleted.calls.at(-1), ['followUp', { content: 'y' }]);
    });

    it('follows up after deferUpdate or update, never overwriting the panel', async () => {
      const afterDeferUpdate = base({ deferred: true, ephemeral: null });
      assert.deepEqual(await safeReply(afterDeferUpdate, { content: 'x' }), { id: 'followed' });
      assert.equal(afterDeferUpdate.calls.some(([name]) => name === 'editReply'), false);
      const replied = base({ replied: true, deferred: false });
      assert.deepEqual(await safeReply(replied, { content: 'x' }), { id: 'followed' });
    });

    it('swallows gone-interaction errors and returns null on any failure', async () => {
      const gone = base({ reply: async () => { throw Object.assign(new Error('Unknown interaction'), { code: 10062 }); } });
      assert.equal(await safeReply(gone, 'x'), null);
      const broken = base({ reply: async () => { throw new Error('boom'); } });
      assert.equal(await safeReply(broken, 'x'), null);
    });

    it('replyError and replySuccess send an embed', async () => {
      const i = base();
      await replyError(i, 'Nope', 'Because');
      assert.equal(i.calls[0][1].ephemeral, true);
      assert.equal(i.calls[0][1].embeds[0].data.title, '❌  Nope');
      const j = base();
      await replySuccess(j, 'Done');
      assert.equal(j.calls[0][1].ephemeral, false);
      assert.equal(j.calls[0][1].embeds[0].data.title, '✅  Done');
    });
  });
});

// ── Embed builder and error formatter ────────────────────────────────────────

describe('embedBuilder', () => {
  it('keeps the factories working and clamps overlong input instead of throwing', () => {
    const e = successEmbed('Done', 'desc', 'foot');
    assert.equal(e.data.title, '✅  Done');
    assert.equal(e.data.description, 'desc');
    assert.equal(e.data.footer.text, 'foot');
    assert.equal(e.data.color, COLORS.SUCCESS);
    assert.ok(e.data.timestamp);
    assert.equal(errorEmbed('x').data.title, '❌  x');
    assert.equal(infoEmbed('x').data.title, 'ℹ️  x');
    assert.equal(warnEmbed('x').data.title, '⚠️  x');
    assert.equal(neutralEmbed('x').data.title, 'x');
    assert.equal(neutralEmbed('x').data.color, COLORS.NEUTRAL);
    assert.equal(infoEmbed('x').data.description, undefined);

    const long = infoEmbed('t'.repeat(400), 'd'.repeat(5000), 'f'.repeat(3000));
    assert.ok(long.data.title.length <= 256);
    assert.ok(long.data.description.length <= 4096);
    assert.ok(long.data.footer.text.length <= 2048);
    assert.doesNotThrow(() => neutralEmbed('t'.repeat(400), 'd'.repeat(5000)));
  });

  it('applyGuildColor and guildEmbed', () => {
    const e = infoEmbed('x');
    assert.equal(applyGuildColor(e, { embedColor: 0x123456 }), e);
    assert.equal(e.data.color, 0x123456);
    assert.equal(applyGuildColor(infoEmbed('x'), null).data.color, COLORS.INFO);
    assert.equal(applyGuildColor(infoEmbed('x'), { embedColor: -1 }).data.color, COLORS.INFO);
    assert.equal(applyGuildColor(infoEmbed('x'), { embedColor: 0x1000000 }).data.color, COLORS.INFO);
    const g = guildEmbed({ embedColor: 0xabcdef }, 'T', 'D', 'F');
    assert.equal(g.data.color, 0xabcdef);
    assert.equal(g.data.title, 'T');
    assert.equal(g.data.footer.text, 'F');
    assert.equal(guildEmbed(null, 'T').data.color, COLORS.INFO);
    assert.equal(guildEmbed({ embedColor: null }).data.color, COLORS.INFO);
    assert.ok(guildEmbed(null, 'T').data.timestamp);
  });

  it('formatError', () => {
    const err = Object.assign(new Error('boom'), { code: 'E_X' });
    const out = formatError(err, { guildId: '1' });
    assert.equal(out.message, 'boom');
    assert.equal(out.name, 'Error');
    assert.equal(out.code, 'E_X');
    assert.equal(out.guildId, '1');
    assert.equal(formatError(new Error('x')).code, null);
    assert.deepEqual(formatError('plain', { a: 1 }), { message: 'plain', a: 1 });
    assert.equal(formatError(new Error('x'), { message: 'override' }).message, 'override');
  });
});

// ── deletionTracker ──────────────────────────────────────────────────────────

describe('deletionTracker', () => {
  it('marks, reads non-destructively and expires', async () => {
    markBotDeletion(['dt-1', 'dt-2'], 'automod');
    assert.equal(wasDeletedByBot('dt-1'), 'automod');
    assert.equal(wasDeletedByBot('dt-1'), 'automod');
    assert.equal(wasDeletedByBot('dt-2'), 'automod');
    markBotDeletion('dt-3', 'x', 1);
    await tick(5);
    assert.equal(wasDeletedByBot('dt-3'), null);
    assert.equal(wasDeletedByBot('dt-never'), null);
  });

  it('accepts iterables and defaults the reason', () => {
    markBotDeletion(new Map([['dt-4', 1], ['dt-5', 2]]).keys(), undefined);
    assert.equal(wasDeletedByBot('dt-4'), 'bot');
    assert.equal(wasDeletedByBot('dt-5'), 'bot');
    markBotDeletion(null, 'x');
    markBotDeletion(42, 'x');
    markBotDeletion('', 'x');
    assert.equal(wasDeletedByBot(''), null);
  });

  it('re-marking refreshes the entry', async () => {
    markBotDeletion('dt-6', 'first', 1);
    markBotDeletion('dt-6', 'second', 60_000);
    await tick(5);
    assert.equal(wasDeletedByBot('dt-6'), 'second');
  });
});

// ── imageSize ────────────────────────────────────────────────────────────────

describe('imageSize', () => {
  const png = (w, h) => {
    const b = Buffer.alloc(24);
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(b);
    b.writeUInt32BE(13, 8);
    b.write('IHDR', 12, 'latin1');
    b.writeUInt32BE(w, 16);
    b.writeUInt32BE(h, 20);
    return b;
  };

  it('reads PNG headers and refuses a decompression bomb', () => {
    assert.deepEqual(readImageSize(png(30000, 30000)), { type: 'png', width: 30000, height: 30000 });
    assert.throws(() => assertImageDimensions(png(30000, 30000)), /IMAGE_TOO_LARGE_OR_UNKNOWN/);
    assert.deepEqual(assertImageDimensions(png(800, 600)), { type: 'png', width: 800, height: 600 });
    assert.throws(() => assertImageDimensions(png(9000, 10)), /IMAGE_TOO_LARGE_OR_UNKNOWN/);
    assert.throws(() => assertImageDimensions(png(5000, 5000)), /IMAGE_TOO_LARGE_OR_UNKNOWN/);
    assert.doesNotThrow(() => assertImageDimensions(png(5000, 5000), { maxPixels: 30_000_000 }));
    assert.throws(() => assertImageDimensions(png(0, 5)), /IMAGE_TOO_LARGE_OR_UNKNOWN/);
    assert.equal(DEFAULT_MAX_SIDE, 8000);
    assert.equal(DEFAULT_MAX_PIXELS, 16_000_000);
  });

  it('returns null for anything unknown or truncated', () => {
    assert.equal(readImageSize(Buffer.from('nope')), null);
    assert.equal(readImageSize(png(1, 1).subarray(0, 20)), null);
    assert.equal(readImageSize(null), null);
    assert.equal(readImageSize(Buffer.alloc(0)), null);
    assert.throws(() => assertImageDimensions(Buffer.from('nope')), /IMAGE_TOO_LARGE_OR_UNKNOWN/);
  });

  it('reads JPEG (skipping other segments), GIF and WebP', () => {
    const app0 = Buffer.from([0xff, 0xe0, 0x00, 0x10, ...Array(14).fill(0)]);
    const sof = Buffer.from([0xff, 0xc0, 0x00, 0x11, 0x08, 0x01, 0x00, 0x02, 0x00, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert.deepEqual(readImageSize(Buffer.concat([Buffer.from([0xff, 0xd8]), app0, sof])), { type: 'jpeg', width: 512, height: 256 });

    const gif = Buffer.alloc(10);
    gif.write('GIF89a', 0, 'latin1');
    gif.writeUInt16LE(320, 6);
    gif.writeUInt16LE(200, 8);
    assert.deepEqual(readImageSize(gif), { type: 'gif', width: 320, height: 200 });

    const vp8x = Buffer.alloc(30);
    vp8x.write('RIFF', 0, 'latin1'); vp8x.write('WEBP', 8, 'latin1'); vp8x.write('VP8X', 12, 'latin1');
    vp8x.writeUIntLE(1023, 24, 3); vp8x.writeUIntLE(767, 27, 3);
    assert.deepEqual(readImageSize(vp8x), { type: 'webp', width: 1024, height: 768 });

    const vp8l = Buffer.alloc(30);
    vp8l.write('RIFF', 0, 'latin1'); vp8l.write('WEBP', 8, 'latin1'); vp8l.write('VP8L', 12, 'latin1');
    vp8l[20] = 0x2f;
    vp8l.writeUInt32LE((99) | (49 << 14), 21);
    assert.deepEqual(readImageSize(vp8l), { type: 'webp', width: 100, height: 50 });

    const vp8 = Buffer.alloc(30);
    vp8.write('RIFF', 0, 'latin1'); vp8.write('WEBP', 8, 'latin1'); vp8.write('VP8 ', 12, 'latin1');
    vp8[23] = 0x9d; vp8[24] = 0x01; vp8[25] = 0x2a;
    vp8.writeUInt16LE(640, 26); vp8.writeUInt16LE(480, 28);
    assert.deepEqual(readImageSize(vp8), { type: 'webp', width: 640, height: 480 });
  });
});

// ── guildMembers ─────────────────────────────────────────────────────────────

describe('guildMembers', () => {
  it('refuses a guild that is too large without fetching', async () => {
    let calls = 0;
    const guild = { id: 'gm-big', memberCount: 200_000, members: { fetch: async () => { calls++; } } };
    assert.deepEqual(await fetchAllMembers(guild), { ok: false, reason: 'too-large' });
    assert.equal(calls, 0);
    assert.equal(FETCH_ALL_MAX_MEMBERS, 100_000);
    assert.deepEqual(await fetchAllMembers({ ...guild, memberCount: 20 }, { maxMembers: 10 }), { ok: false, reason: 'too-large' });
  });

  it('shares one in-flight fetch between concurrent callers', async () => {
    let calls = 0;
    const members = new Map([['1', { id: '1' }]]);
    const guild = { id: 'gm-shared', memberCount: 1, members: { fetch: async () => { calls++; await tick(20); return members; } } };
    const [a, b] = await Promise.all([fetchAllMembers(guild), fetchAllMembers(guild)]);
    assert.equal(calls, 1);
    assert.deepEqual(a, { ok: true, members });
    assert.deepEqual(b, { ok: true, members });
    await fetchAllMembers(guild);
    assert.equal(calls, 2, 'a later call fetches again');
  });

  it('remembers gateway rate limits per guild', async () => {
    let calls = 0;
    const guild = {
      id: 'gm-limited', memberCount: 1,
      members: { fetch: async () => { calls++; throw Object.assign(new Error('rate limited'), { name: 'GatewayRateLimitError', data: { retry_after: 2 } }); } },
    };
    const first = await fetchAllMembers(guild);
    assert.equal(first.ok, false);
    assert.equal(first.reason, 'rate-limited');
    assert.equal(first.retryAfterMs, 2000);
    const second = await fetchAllMembers(guild);
    assert.equal(second.reason, 'rate-limited');
    assert.ok(second.retryAfterMs > 0 && second.retryAfterMs <= 2000);
    assert.equal(calls, 1);
  });

  it('times out and reports errors', async () => {
    const hung = { id: 'gm-hung', memberCount: 1, members: { fetch: () => new Promise(() => {}) } };
    assert.deepEqual(await fetchAllMembers(hung, { timeoutMs: 20 }), { ok: false, reason: 'timeout' });
    const broken = { id: 'gm-broken', memberCount: 1, members: { fetch: async () => { throw new Error('boom'); } } };
    assert.deepEqual(await fetchAllMembers(broken), { ok: false, reason: 'error' });
    assert.deepEqual(await fetchAllMembers(null), { ok: false, reason: 'error' });
  });
});

// ── Paginator and confirm dialog (real InteractionCollector, mock client) ────

function makeClient() {
  const client = new EventEmitter();
  client.incrementMaxListeners = () => {};
  client.decrementMaxListeners = () => {};
  client.options = {};
  return client;
}

// Builds a mock message whose collector is the real discord.js InteractionCollector.
function makeMessage(client, log, { id = 'MSG1' } = {}) {
  return {
    id, channelId: 'CH1', guildId: 'G1', client,
    createMessageComponentCollector(options) {
      return new InteractionCollector(client, { ...options, interactionType: InteractionType.MessageComponent, message: this });
    },
    async edit(payload) { log.push({ via: 'message.edit', payload, at: Date.now() }); },
  };
}

// An interaction target. `deferred: true` models deferReply (visibility fixed at defer time).
function makeTarget(message, log, { deferred = false, repliable = true } = {}) {
  const target = {
    user: { id: 'INVOKER' }, author: { id: 'INVOKER' },
    deferred, replied: false, ephemeral: deferred ? true : null,
    createdTimestamp: Date.now(),
    isRepliable: () => true,
    async reply(payload) { log.push({ via: 'reply', payload }); target.replied = true; return { resource: { message } }; },
    async followUp(payload) { log.push({ via: 'followUp', payload }); return message; },
    async editReply(payload) { log.push({ via: 'editReply', payload, at: Date.now() }); return message; },
  };
  if (!repliable) {
    delete target.isRepliable;
    delete target.user;
    delete target.deferred;
    target.reply = async (payload) => { log.push({ via: 'message.reply', payload }); return message; };
  }
  return target;
}

// Emits a button click and records how it was answered.
function click(client, message, log, customId, userId = 'INVOKER', { at = Date.now() } = {}) {
  const calls = [];
  const i = {
    id: `I${Math.random()}`, type: InteractionType.MessageComponent, componentType: ComponentType.Button,
    message, channelId: 'CH1', guildId: 'G1', customId, user: { id: userId },
    createdTimestamp: at, replied: false, deferred: false,
    async update(payload) { calls.push(['update', payload]); i.replied = true; },
    async deferUpdate() { calls.push(['deferUpdate']); i.deferred = true; },
    async reply(payload) { calls.push(['reply', payload]); i.replied = true; },
    async editReply(payload) { log.push({ via: 'click.editReply', payload, at: Date.now() }); },
  };
  client.emit('interactionCreate', i);
  return calls;
}

const pagesOf = (n, footer) => Array.from({ length: n }, (_, k) => {
  const e = new EmbedBuilder().setTitle(`page ${k + 1}`);
  if (footer) e.setFooter({ text: footer });
  return e;
});
const footerOf = (payload) => payload.embeds[0].data.footer?.text;
const allDisabled = (payload) => payload.components[0].components.every((c) => c.data.disabled === true);

describe('paginator', () => {
  it('sends a single page without buttons and never mutates the caller\'s embeds', async () => {
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    const pages = pagesOf(1, 'only');
    const sent = await paginate(makeTarget(message, log), pages);
    assert.equal(sent, message);
    assert.deepEqual(log[0].payload.components, []);
    assert.equal(footerOf(log[0].payload), 'only');
    assert.equal(await paginate(makeTarget(message, log), []), null);
    assert.equal(await paginate(makeTarget(message, log), null), null);
    assert.equal(client.listenerCount('interactionCreate'), 0, 'no collector for a single page');
  });

  it('appends the page indicator without accumulating, and leaves the builders alone', async () => {
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    const pages = pagesOf(3, '7 records');
    await paginate(makeTarget(message, log), pages);
    assert.equal(footerOf(log[0].payload), '7 records • Page 1/3');
    const next = click(client, message, log, 'paginator:next');
    await tick();
    assert.equal(footerOf(next[0][1]), '7 records • Page 2/3');
    const prev = click(client, message, log, 'paginator:prev');
    await tick();
    assert.equal(footerOf(prev[0][1]), '7 records • Page 1/3');
    const next2 = click(client, message, log, 'paginator:next');
    await tick();
    assert.equal(footerOf(next2[0][1]), '7 records • Page 2/3');
    assert.equal(pages[0].data.footer.text, '7 records');
    assert.equal(pages[1].data.footer.text, '7 records');
    click(client, message, log, 'paginator:stop');
    await tick();
  });

  it('sets a footer when there is none, honours replace and none, and keeps an existing indicator', async () => {
    for (const [mode, expected] of [['append', 'Page 1 of 2'], ['replace', 'Page 1 of 2'], ['none', undefined]]) {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      await paginate(makeTarget(message, log), pagesOf(2), { footer: mode });
      assert.equal(footerOf(log[0].payload), expected, mode);
      click(client, message, log, 'paginator:stop');
      await tick();
    }
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    await paginate(makeTarget(message, log), pagesOf(3, '12 total record(s) · Page 1/3'));
    assert.equal(footerOf(log[0].payload), '12 total record(s) · Page 1/3');
    click(client, message, log, 'paginator:stop');
    await tick();
    const client2 = makeClient(); const log2 = [];
    const message2 = makeMessage(client2, log2);
    await paginate(makeTarget(message2, log2), pagesOf(3, 'Page 1 of 3 · Top 200 of 400 members'));
    assert.equal(footerOf(log2[0].payload), 'Page 1 of 3 · Top 200 of 400 members');
    click(client2, message2, log2, 'paginator:stop');
    await tick();
  });

  it('Stop is acknowledged by exactly one update with disabled buttons, and nothing is edited afterwards', async () => {
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    await paginate(makeTarget(message, log), pagesOf(3));
    const acks = click(client, message, log, 'paginator:stop');
    await tick(50);
    assert.equal(acks.length, 1);
    assert.equal(acks[0][0], 'update');
    assert.ok(allDisabled(acks[0][1]));
    assert.deepEqual(log.map((entry) => entry.via), ['reply'], 'no end edit after Stop');
    assert.equal(client.listenerCount('interactionCreate'), 0, 'collector removed its listener');
  });

  it('Prev and Next acknowledge with update and clamp at the ends', async () => {
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    await paginate(makeTarget(message, log), pagesOf(2), { startPage: 5 });
    assert.equal(log[0].payload.embeds[0].data.title, 'page 2');
    assert.equal(log[0].payload.components[0].components[2].data.disabled, true);
    const acks = click(client, message, log, 'paginator:prev');
    await tick();
    assert.equal(acks[0][1].embeds[0].data.title, 'page 1');
    assert.equal(acks[0][1].components[0].components[0].data.disabled, true);
    const again = click(client, message, log, 'paginator:prev');
    await tick();
    assert.equal(again[0][1].embeds[0].data.title, 'page 1');
    click(client, message, log, 'paginator:stop');
    await tick();
  });

  it('answers a stranger without changing the page', async () => {
    const client = makeClient(); const log = [];
    const message = makeMessage(client, log);
    await paginate(makeTarget(message, log), pagesOf(3));
    const acks = click(client, message, log, 'paginator:next', 'STRANGER');
    await tick();
    assert.equal(acks.length, 1);
    assert.equal(acks[0][0], 'reply');
    assert.equal(acks[0][1].ephemeral, true);
    assert.equal(acks[0][1].content, "These buttons aren't for you.");
    const other = click(client, message, log, 'someone-else:button', 'STRANGER');
    await tick();
    assert.equal(other.length, 0, 'non-paginator ids are not answered');
    const mine = click(client, message, log, 'paginator:next');
    await tick();
    assert.equal(mine[0][1].embeds[0].data.title, 'page 2');
    click(client, message, log, 'paginator:stop');
    await tick();
  });

  describe('when it idles out', { concurrency: true }, () => {
    const IDLE = 5_000;

    it('a stranger cannot keep it alive', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      const started = Date.now();
      await paginate(makeTarget(message, log), pagesOf(3), { timeout: IDLE });
      await tick(2_500);
      click(client, message, log, 'paginator:next', 'STRANGER');
      await tick(IDLE - 2_500 + 800);
      const end = log.find((entry) => entry.via === 'editReply');
      assert.ok(end, 'the end edit happened');
      assert.ok(end.at - started < IDLE + 700, `ended at ${end.at - started} ms, not extended by the stranger`);
      assert.equal(end.payload.message, 'MSG1');
      assert.ok(allDisabled(end.payload));
      assert.equal(log.some((entry) => entry.via === 'message.edit'), false);
    });

    it('an ephemeral paginator edits through the interaction webhook, not the channel route', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      const target = makeTarget(message, log, { deferred: true });
      await paginate(target, pagesOf(3), { timeout: IDLE, ephemeral: true });
      assert.equal(log[0].via, 'editReply', 'a deferred interaction is edited, keeping its visibility');
      await tick(IDLE + 800);
      const end = log.at(-1);
      assert.equal(end.via, 'editReply');
      assert.equal(end.payload.message, 'MSG1');
      assert.ok(allDisabled(end.payload));
      assert.equal(log.some((entry) => entry.via === 'message.edit'), false);
    });

    it('followUp mode sends with followUp and ends through editReply on the message id', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log, { id: 'FOLLOW1' });
      const target = makeTarget(message, log, { deferred: true });
      await paginate(target, pagesOf(3), { timeout: IDLE, followUp: true });
      assert.equal(log[0].via, 'followUp');
      assert.equal(log.some((entry) => entry.via === 'editReply' && !entry.payload.message), false, 'never @original');
      await tick(IDLE + 800);
      const end = log.at(-1);
      assert.equal(end.via, 'editReply');
      assert.equal(end.payload.message, 'FOLLOW1');
      assert.equal(log.some((entry) => entry.via === 'message.edit'), false);
    });

    it('after a click, the end edit goes through that click', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      await paginate(makeTarget(message, log), pagesOf(3), { timeout: IDLE });
      click(client, message, log, 'paginator:next');
      await tick(IDLE + 800);
      const end = log.at(-1);
      assert.equal(end.via, 'click.editReply');
      assert.ok(allDisabled(end.payload));
      assert.equal(log.some((entry) => entry.via === 'editReply'), false);
    });

    it('a message target ends with message.edit', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      const target = makeTarget(message, log, { repliable: false });
      await paginate(target, pagesOf(3), { timeout: IDLE });
      assert.equal(log[0].via, 'message.reply');
      await tick(IDLE + 800);
      assert.equal(log.at(-1).via, 'message.edit');
      assert.ok(allDisabled(log.at(-1).payload));
    });

    it('awaitConfirm answers a stranger, and reports a timeout as false', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log, { id: 'CONF1' });
      const target = makeTarget(message, log);
      target.user = { id: 'INVOKER' };
      const result = awaitConfirm(target, { title: 'Sure?', description: 'This cannot be undone.', timeoutMs: IDLE });
      await tick(50);
      const sent = log[0].payload;
      const ids = sent.components[0].components.map((c) => c.data.custom_id);
      assert.match(ids[0], /^confirm:[0-9a-f]{12}:yes$/);
      assert.match(ids[1], /^confirm:[0-9a-f]{12}:no$/);
      assert.equal(sent.ephemeral, true);
      const stranger = click(client, message, log, ids[0], 'STRANGER');
      await tick();
      assert.equal(stranger[0][0], 'reply');
      assert.equal(stranger[0][1].content, "These buttons aren't for you.");
      assert.equal(await result, false);
      const end = log.at(-1);
      assert.equal(end.via, 'editReply');
      assert.equal(end.payload.message, 'CONF1');
      assert.deepEqual(end.payload.components, []);
      assert.match(end.payload.embeds[0].data.footer.text, /^Timed out/);
    });
  });

  describe('awaitConfirm', () => {
    it('resolves true for the invoker confirming and removes the buttons', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      const target = makeTarget(message, log);
      const result = awaitConfirm(target, { title: 'Delete?' });
      await tick(30);
      const yes = log[0].payload.components[0].components[0].data.custom_id;
      const acks = click(client, message, log, yes);
      assert.equal(await result, true);
      assert.equal(acks[0][0], 'update');
      assert.deepEqual(acks[0][1].components, []);
      assert.equal(acks[0][1].embeds[0].data.footer.text, 'Confirmed');
    });

    it('resolves false for cancel', async () => {
      const client = makeClient(); const log = [];
      const message = makeMessage(client, log);
      const target = makeTarget(message, log);
      const result = awaitConfirm(target, { title: 'Delete?', danger: false });
      await tick(30);
      const no = log[0].payload.components[0].components[1].data.custom_id;
      const acks = click(client, message, log, no);
      assert.equal(await result, false);
      assert.equal(acks[0][1].embeds[0].data.footer.text, 'Cancelled');
    });

    it('uses editReply after deferReply and followUp after an earlier reply', async () => {
      for (const [state, via] of [[{ deferred: true }, 'editReply'], [{ replied: true }, 'followUp']]) {
        const client = makeClient(); const log = [];
        const message = makeMessage(client, log);
        const target = makeTarget(message, log);
        Object.assign(target, state, state.deferred ? { ephemeral: false } : {});
        const result = awaitConfirm(target, { title: 'x' });
        await tick(30);
        assert.equal(log[0].via, via);
        click(client, message, log, log[0].payload.components[0].components[1].data.custom_id);
        assert.equal(await result, false);
      }
    });

    it('resolves false instead of throwing when sending fails', async () => {
      const target = { user: { id: 'u' }, deferred: false, replied: false, reply: async () => { throw new Error('boom'); } };
      assert.equal(await awaitConfirm(target, { title: 'x' }), false);
    });
  });
});
